use acvm::brillig_vm::{brillig::Value, Registers};
use fm::FileManager;
use noirc_driver::{compile_brillig_main, prepare_crate, CompileOptions};
use noirc_errors::FileDiagnostic;
use noirc_evaluator::brillig::brillig_ir::artifact::GeneratedBrillig;
use noirc_frontend::{graph::CrateGraph, hir::Context};
use std::path::Path;

/// Compile program from file using path.
pub(crate) fn compile(
    entry_point: &str,
) -> Result<(GeneratedBrillig, Registers, Vec<Value>), Vec<FileDiagnostic>> {
    let options = CompileOptions::default();

    let path = std::env::current_dir().expect("No current directory");
    let root = Path::new(&path);
    let fm = FileManager::new(root, Box::new(|path| std::fs::read_to_string(path)));
    let graph = CrateGraph::default();
    let mut context = Context::new(fm, graph);

    let path = Path::new(entry_point);
    let crate_id = prepare_crate(&mut context, path);

    let compiled_brillig_parts = compile_brillig_main(&mut context, crate_id, &options, None)?.0;
    Ok((compiled_brillig_parts.0, compiled_brillig_parts.1, compiled_brillig_parts.2))
}
