#![allow(non_upper_case_globals)]

use crate::runtime::{show_pairing, start_bridge, stop_bridge};
use pyo3::prelude::*;
use pyo3::types::PyDict;

#[pyclass]
pub struct Workbench;

#[pymethods]
impl Workbench {
    #[classattr]
    const MenuText: &'static str = "Topomind Semantic MCP";
    #[classattr]
    const ToolTip: &'static str = "Revisioned semantic CAD context and safe agentic editing";
    #[classattr]
    const Icon: &'static str = "";

    #[pyo3(name = "Initialize")]
    fn initialize(&self, py: Python<'_>) -> PyResult<()> {
        let gui = PyModule::import(py, "FreeCADGui")?;
        gui.getattr("addCommand")?
            .call1(("Topomind_StartBridge", Py::new(py, StartCommand)?))?;
        gui.getattr("addCommand")?
            .call1(("Topomind_StopBridge", Py::new(py, StopCommand)?))?;
        gui.getattr("addCommand")?
            .call1(("Topomind_ShowPairing", Py::new(py, PairingCommand)?))?;
        if let Ok(add_menu) = gui.getattr("addMenu") {
            let _ = add_menu.call1(("Topomind",));
        }
        Ok(())
    }

    #[pyo3(name = "GetClassName")]
    fn class_name(&self) -> &'static str {
        "Gui::PythonWorkbench"
    }
}

#[pyclass]
struct StartCommand;

#[pymethods]
impl StartCommand {
    #[pyo3(name = "Activated")]
    fn activated(&self, py: Python<'_>) -> PyResult<()> {
        start_bridge(py).map(|_| ())
    }
    #[pyo3(name = "GetResources")]
    fn resources(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        resources(
            py,
            "Start bridge",
            "Start the authenticated local Topomind bridge",
        )
    }
}

#[pyclass]
struct StopCommand;

#[pymethods]
impl StopCommand {
    #[pyo3(name = "Activated")]
    fn activated(&self, py: Python<'_>) {
        stop_bridge(py);
    }
    #[pyo3(name = "GetResources")]
    fn resources(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        resources(py, "Stop bridge", "Stop the Topomind bridge")
    }
}

#[pyclass]
struct PairingCommand;

#[pymethods]
impl PairingCommand {
    #[pyo3(name = "Activated")]
    fn activated(&self, py: Python<'_>) -> PyResult<()> {
        show_pairing(py).map(|_| ())
    }
    #[pyo3(name = "GetResources")]
    fn resources(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        resources(
            py,
            "Show pairing status",
            "Show local Topomind pairing status",
        )
    }
}

fn resources(py: Python<'_>, menu: &str, tooltip: &str) -> PyResult<Py<PyAny>> {
    let dictionary = PyDict::new(py);
    dictionary.set_item("MenuText", menu)?;
    dictionary.set_item("ToolTip", tooltip)?;
    Ok(dictionary.unbind().into_any())
}

pub fn add_workbench(py: Python<'_>) -> PyResult<()> {
    let gui = PyModule::import(py, "FreeCADGui")?;
    gui.getattr("addWorkbench")?
        .call1((Py::new(py, Workbench)?,))?;
    Ok(())
}
