#![allow(non_snake_case)]
#![allow(nonstandard_style)]

mod executor;
mod extractor;
mod pyutil;
mod runtime;
mod workbench;

use pyo3::prelude::*;

#[pyfunction]
fn install(py: Python<'_>) -> PyResult<()> {
    workbench::add_workbench(py)
}

#[pyfunction]
fn start_bridge(py: Python<'_>) -> PyResult<String> {
    runtime::start_bridge(py)
}

#[pyfunction]
fn stop_bridge(py: Python<'_>) -> PyResult<()> {
    runtime::stop_bridge(py);
    Ok(())
}

#[pyfunction]
fn pairing_status(py: Python<'_>) -> PyResult<Py<PyAny>> {
    runtime::pairing_status(py)
}

#[pymodule]
fn SemanticMCP_native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(install, module)?)?;
    module.add_function(wrap_pyfunction!(start_bridge, module)?)?;
    module.add_function(wrap_pyfunction!(stop_bridge, module)?)?;
    module.add_function(wrap_pyfunction!(pairing_status, module)?)?;
    Ok(())
}
