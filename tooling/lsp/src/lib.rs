#![forbid(unsafe_code)]
#![warn(unreachable_pub)]
#![warn(clippy::semicolon_if_nothing_returned)]
#![cfg_attr(not(test), warn(unused_crate_dependencies, unused_extern_crates))]

use std::{
    future::Future,
    ops::{self, ControlFlow},
    path::{Path, PathBuf},
    pin::Pin,
    task::{self, Poll},
};

use acvm::BlackBoxFunctionSolver;
use async_lsp::{
    router::Router, AnyEvent, AnyNotification, AnyRequest, ClientSocket, Error, ErrorCode,
    LanguageClient, LspService, ResponseError,
};
use codelens::{on_code_lens_request, on_test_run_request, on_tests_request};
use codespan_reporting::files;
use fm::FILE_EXTENSION;
use nargo::prepare_package;
use nargo_toml::{find_package_manifest, resolve_workspace_from_toml, PackageSelection};
use noirc_driver::check_crate;
use noirc_errors::{DiagnosticKind, FileDiagnostic};
use noirc_frontend::{
    graph::{CrateId, CrateName},
    hir::{Context, FunctionNameMatch},
};
use serde_json::Value as JsonValue;
use tower::Service;

mod codelens;
mod types;

use types::{
    notification, request, CodeLensOptions, Diagnostic, DiagnosticSeverity,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, LogMessageParams, MessageType, NargoCapability, NargoPackageTests,
    NargoTest, NargoTestId, NargoTestsOptions, Position, PublishDiagnosticsParams, Range,
    ServerCapabilities, TextDocumentSyncOptions, Url,
};

// This is a struct that wraps a dynamically dispatched `BlackBoxFunctionSolver`
// where we proxy the unimplemented stuff to the wrapped backend, but it
// allows us to avoid changing function signatures to include the `Box`
struct WrapperSolver(Box<dyn BlackBoxFunctionSolver>);

impl BlackBoxFunctionSolver for WrapperSolver {
    fn schnorr_verify(
        &self,
        public_key_x: &acvm::FieldElement,
        public_key_y: &acvm::FieldElement,
        signature: &[u8],
        message: &[u8],
    ) -> Result<bool, acvm::BlackBoxResolutionError> {
        self.0.schnorr_verify(public_key_x, public_key_y, signature, message)
    }

    fn pedersen(
        &self,
        inputs: &[acvm::FieldElement],
        domain_separator: u32,
    ) -> Result<(acvm::FieldElement, acvm::FieldElement), acvm::BlackBoxResolutionError> {
        self.0.pedersen(inputs, domain_separator)
    }

    fn fixed_base_scalar_mul(
        &self,
        low: &acvm::FieldElement,
        high: &acvm::FieldElement,
    ) -> Result<(acvm::FieldElement, acvm::FieldElement), acvm::BlackBoxResolutionError> {
        self.0.fixed_base_scalar_mul(low, high)
    }
}

// State for the LSP gets implemented on this struct and is internal to the implementation
pub struct LspState {
    root_path: Option<PathBuf>,
    client: ClientSocket,
    solver: WrapperSolver,
}

impl LspState {
    fn new(client: &ClientSocket, solver: impl BlackBoxFunctionSolver + 'static) -> Self {
        Self { client: client.clone(), root_path: None, solver: WrapperSolver(Box::new(solver)) }
    }
}

pub struct NargoLspService {
    router: Router<LspState>,
}

impl NargoLspService {
    pub fn new(client: &ClientSocket, solver: impl BlackBoxFunctionSolver + 'static) -> Self {
        let state = LspState::new(client, solver);
        let mut router = Router::new(state);
        router
            .request::<request::Initialize, _>(on_initialize)
            .request::<request::Shutdown, _>(on_shutdown)
            .request::<request::CodeLens, _>(on_code_lens_request)
            .request::<request::NargoTests, _>(on_tests_request)
            .request::<request::NargoTestRun, _>(on_test_run_request)
            .notification::<notification::Initialized>(on_initialized)
            .notification::<notification::DidChangeConfiguration>(on_did_change_configuration)
            .notification::<notification::DidOpenTextDocument>(on_did_open_text_document)
            .notification::<notification::DidChangeTextDocument>(on_did_change_text_document)
            .notification::<notification::DidCloseTextDocument>(on_did_close_text_document)
            .notification::<notification::DidSaveTextDocument>(on_did_save_text_document)
            .notification::<notification::Exit>(on_exit);
        Self { router }
    }
}

// This trait implemented as a passthrough to the router, which makes
// our `NargoLspService` a normal Service as far as Tower is concerned.
impl Service<AnyRequest> for NargoLspService {
    type Response = JsonValue;
    type Error = ResponseError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.router.poll_ready(cx)
    }

    fn call(&mut self, req: AnyRequest) -> Self::Future {
        self.router.call(req)
    }
}

// This trait implemented as a passthrough to the router, which makes
// our `NargoLspService` able to accept the `async-lsp` middleware.
impl LspService for NargoLspService {
    fn notify(&mut self, notification: AnyNotification) -> ControlFlow<Result<(), Error>> {
        self.router.notify(notification)
    }

    fn emit(&mut self, event: AnyEvent) -> ControlFlow<Result<(), Error>> {
        self.router.emit(event)
    }
}

// Handlers
// The handlers for `request` are not `async` because it compiles down to lifetimes that can't be added to
// the router. To return a future that fits the trait, it is easiest wrap your implementations in an `async {}`
// block but you can also use `std::future::ready`.
//
// Additionally, the handlers for `notification` aren't async at all.
//
// They are not attached to the `NargoLspService` struct so they can be unit tested with only `LspState`
// and params passed in.

fn on_initialize(
    state: &mut LspState,
    params: InitializeParams,
) -> impl Future<Output = Result<InitializeResult, ResponseError>> {
    state.root_path = params.root_uri.and_then(|root_uri| root_uri.to_file_path().ok());

    async {
        let text_document_sync =
            TextDocumentSyncOptions { save: Some(true.into()), ..Default::default() };

        let code_lens = CodeLensOptions { resolve_provider: Some(false) };

        let nargo = NargoCapability {
            tests: Some(NargoTestsOptions {
                fetch: Some(true),
                run: Some(true),
                update: Some(true),
            }),
        };

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(text_document_sync.into()),
                code_lens_provider: Some(code_lens),
                nargo: Some(nargo),
            },
            server_info: None,
        })
    }
}

fn on_shutdown(
    _state: &mut LspState,
    _params: (),
) -> impl Future<Output = Result<(), ResponseError>> {
    async { Ok(()) }
}

fn on_initialized(
    _state: &mut LspState,
    _params: InitializedParams,
) -> ControlFlow<Result<(), async_lsp::Error>> {
    ControlFlow::Continue(())
}

fn on_did_change_configuration(
    _state: &mut LspState,
    _params: DidChangeConfigurationParams,
) -> ControlFlow<Result<(), async_lsp::Error>> {
    ControlFlow::Continue(())
}

fn on_did_open_text_document(
    _state: &mut LspState,
    _params: DidOpenTextDocumentParams,
) -> ControlFlow<Result<(), async_lsp::Error>> {
    ControlFlow::Continue(())
}

fn on_did_change_text_document(
    _state: &mut LspState,
    _params: DidChangeTextDocumentParams,
) -> ControlFlow<Result<(), async_lsp::Error>> {
    ControlFlow::Continue(())
}

fn on_did_close_text_document(
    _state: &mut LspState,
    _params: DidCloseTextDocumentParams,
) -> ControlFlow<Result<(), async_lsp::Error>> {
    ControlFlow::Continue(())
}

fn on_did_save_text_document(
    state: &mut LspState,
    params: DidSaveTextDocumentParams,
) -> ControlFlow<Result<(), async_lsp::Error>> {
    let file_path = match params.text_document.uri.to_file_path() {
        Ok(file_path) => file_path,
        Err(()) => {
            return ControlFlow::Break(Err(ResponseError::new(
                ErrorCode::REQUEST_FAILED,
                "URI is not a valid file path",
            )
            .into()))
        }
    };

    let root_path = match &state.root_path {
        Some(root) => root,
        None => {
            return ControlFlow::Break(Err(ResponseError::new(
                ErrorCode::REQUEST_FAILED,
                "Could not find project root",
            )
            .into()));
        }
    };

    let toml_path = match find_package_manifest(root_path, &file_path) {
        Ok(toml_path) => toml_path,
        Err(err) => {
            // If we cannot find a manifest, we log a warning but return no diagnostics
            // We can reconsider this when we can build a file without the need for a Nargo.toml file to resolve deps
            let _ = state.client.log_message(LogMessageParams {
                typ: MessageType::WARNING,
                message: format!("{err}"),
            });
            return ControlFlow::Continue(());
        }
    };
    let workspace = match resolve_workspace_from_toml(&toml_path, PackageSelection::All) {
        Ok(workspace) => workspace,
        Err(err) => {
            // If we found a manifest, but the workspace is invalid, we raise an error about it
            return ControlFlow::Break(Err(ResponseError::new(
                ErrorCode::REQUEST_FAILED,
                format!("{err}"),
            )
            .into()));
        }
    };

    let mut diagnostics = Vec::new();

    for package in &workspace {
        let (mut context, crate_id) =
            prepare_package(package, Box::new(|path| std::fs::read_to_string(path)));

        let file_diagnostics = match check_crate(&mut context, crate_id, false) {
            Ok(((), warnings)) => warnings,
            Err(errors_and_warnings) => errors_and_warnings,
        };

        // We don't add test headings for a package if it contains no `#[test]` functions
        if let Some(tests) = get_package_tests_in_crate(&context, &crate_id, &package.name) {
            let _ = state.client.notify::<notification::NargoUpdateTests>(NargoPackageTests {
                package: package.name.to_string(),
                tests,
            });
        }

        if !file_diagnostics.is_empty() {
            let fm = &context.file_manager;
            let files = fm.as_file_map();

            for FileDiagnostic { file_id, diagnostic, call_stack: _ } in file_diagnostics {
                // Ignore diagnostics for any file that wasn't the file we saved
                // TODO: In the future, we could create "related" diagnostics for these files
                // TODO: This currently just appends the `.nr` file extension that we store as a constant,
                // but that won't work if we accept other extensions
                if fm.path(file_id).with_extension(FILE_EXTENSION) != file_path {
                    continue;
                }

                let mut range = Range::default();

                // TODO: Should this be the first item in secondaries? Should we bail when we find a range?
                for sec in diagnostic.secondaries {
                    // Not using `unwrap_or_default` here because we don't want to overwrite a valid range with a default range
                    if let Some(r) = byte_span_to_range(files, file_id, sec.span.into()) {
                        range = r;
                    }
                }
                let severity = match diagnostic.kind {
                    DiagnosticKind::Error => Some(DiagnosticSeverity::ERROR),
                    DiagnosticKind::Warning => Some(DiagnosticSeverity::WARNING),
                };
                diagnostics.push(Diagnostic {
                    range,
                    severity,
                    message: diagnostic.message,
                    ..Default::default()
                });
            }
        }
    }

    // We need to refresh lenses when we compile since that's the only time they can be accurately reflected
    std::mem::drop(state.client.code_lens_refresh(()));

    let _ = state.client.publish_diagnostics(PublishDiagnosticsParams {
        uri: params.text_document.uri,
        version: None,
        diagnostics,
    });

    ControlFlow::Continue(())
}

fn on_exit(_state: &mut LspState, _params: ()) -> ControlFlow<Result<(), async_lsp::Error>> {
    ControlFlow::Continue(())
}

fn get_package_tests_in_crate(
    context: &Context,
    crate_id: &CrateId,
    crate_name: &CrateName,
) -> Option<Vec<NargoTest>> {
    let fm = &context.file_manager;
    let files = fm.as_file_map();
    let tests =
        context.get_all_test_functions_in_crate_matching(crate_id, FunctionNameMatch::Anything);

    let mut package_tests = Vec::new();

    for (func_name, test_function) in tests {
        let location = context.function_meta(&test_function.get_id()).name.location;
        let file_id = location.file;

        let file_path = fm.path(file_id).with_extension(FILE_EXTENSION);
        let range = byte_span_to_range(files, file_id, location.span.into()).unwrap_or_default();

        package_tests.push(NargoTest {
            id: NargoTestId::new(crate_name.clone(), func_name.clone()),
            label: func_name,
            uri: Url::from_file_path(file_path)
                .expect("Expected a valid file path that can be converted into a URI"),
            range,
        });
    }

    if package_tests.is_empty() {
        None
    } else {
        Some(package_tests)
    }
}

fn byte_span_to_range<'a, F: files::Files<'a> + ?Sized>(
    files: &'a F,
    file_id: F::FileId,
    span: ops::Range<usize>,
) -> Option<Range> {
    if let Ok(codespan_range) = codespan_lsp::byte_span_to_range(files, file_id, span) {
        // We have to manually construct a Range because the codespan_lsp restricts lsp-types to the wrong version range
        // TODO: codespan is unmaintained and we should probably subsume it. Ref https://github.com/brendanzab/codespan/issues/345
        let range = Range {
            start: Position {
                line: codespan_range.start.line,
                character: codespan_range.start.character,
            },
            end: Position {
                line: codespan_range.end.line,
                character: codespan_range.end.character,
            },
        };
        Some(range)
    } else {
        None
    }
}

#[cfg(test)]
mod lsp_tests {
    use lsp_types::TextDocumentSyncCapability;
    use tokio::test;

    use super::*;

    #[test]
    async fn test_on_initialize() {
        struct MockBackend;
        impl BlackBoxFunctionSolver for MockBackend {
            fn schnorr_verify(
                &self,
                _public_key_x: &acvm::FieldElement,
                _public_key_y: &acvm::FieldElement,
                _signature: &[u8],
                _message: &[u8],
            ) -> Result<bool, acvm::BlackBoxResolutionError> {
                unimplemented!()
            }

            fn pedersen(
                &self,
                _inputs: &[acvm::FieldElement],
                _domain_separator: u32,
            ) -> Result<(acvm::FieldElement, acvm::FieldElement), acvm::BlackBoxResolutionError>
            {
                unimplemented!()
            }

            fn fixed_base_scalar_mul(
                &self,
                _low: &acvm::FieldElement,
                _high: &acvm::FieldElement,
            ) -> Result<(acvm::FieldElement, acvm::FieldElement), acvm::BlackBoxResolutionError>
            {
                unimplemented!()
            }
        }

        let client = ClientSocket::new_closed();
        let solver = MockBackend;
        let mut state = LspState::new(&client, solver);
        let params = InitializeParams::default();
        let response = on_initialize(&mut state, params).await.unwrap();
        assert!(matches!(
            response.capabilities,
            ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions { save: Some(_), .. }
                )),
                code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) }),
                ..
            }
        ));
        assert!(response.server_info.is_none());
    }
}

cfg_if::cfg_if! {
    if #[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))] {
        use wasm_bindgen::{prelude::*, JsValue};

        #[wasm_bindgen(module = "@noir-lang/source-resolver")]
        extern "C" {

            #[wasm_bindgen(catch)]
            fn read_file(path: &str) -> Result<String, JsValue>;

        }

        fn get_non_stdlib_asset(path_to_file: &Path) -> std::io::Result<String> {
            let path_str = path_to_file.to_str().unwrap();
            match read_file(path_str) {
                Ok(buffer) => Ok(buffer),
                Err(_) => Err(Error::new(ErrorKind::Other, "could not read file using wasm")),
            }
        }
    } else {
        fn get_non_stdlib_asset(path_to_file: &Path) -> std::io::Result<String> {
            std::fs::read_to_string(path_to_file)
        }
    }
}
