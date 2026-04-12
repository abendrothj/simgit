use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use simgit_sdk::{Client, DiffResult, SessionInfo};
use uuid::Uuid;

fn py_err<E: std::fmt::Display>(e: E) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Module-level Tokio runtime shared across all Python → Rust async calls.
///
/// Creating a new runtime per call adds ~1 ms overhead and GC pressure.
/// A single cached runtime eliminates this cost. `OnceLock` guarantees
/// thread-safe one-time initialization.
static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn runtime() -> PyResult<&'static tokio::runtime::Runtime> {
    RUNTIME.get_or_try_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(py_err)
    })
}

fn run_async<F, T>(fut: F) -> PyResult<T>
where
    F: std::future::Future<Output = Result<T, simgit_sdk::SdkError>>,
{
    runtime()?.block_on(fut).map_err(py_err)
}

fn session_info_dict(py: Python<'_>, info: &SessionInfo) -> PyResult<Py<PyDict>> {
    let d = PyDict::new_bound(py);
    d.set_item("session_id", info.session_id.to_string())?;
    d.set_item("task_id", &info.task_id)?;
    d.set_item("agent_label", &info.agent_label)?;
    d.set_item("base_commit", &info.base_commit)?;
    d.set_item("created_at", info.created_at.to_rfc3339())?;
    d.set_item("status", format!("{:?}", info.status).to_uppercase())?;
    d.set_item("mount_path", info.mount_path.to_string_lossy().to_string())?;
    d.set_item("branch_name", &info.branch_name)?;
    d.set_item("peers_enabled", info.peers_enabled)?;
    Ok(d.unbind())
}

fn diff_result_dict(py: Python<'_>, diff: &DiffResult) -> PyResult<Py<PyDict>> {
    let d = PyDict::new_bound(py);
    let changed_paths: Vec<String> = diff
        .changed_paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    d.set_item("session_id", diff.session_id.to_string())?;
    d.set_item("unified_diff", &diff.unified_diff)?;
    d.set_item("changed_paths", changed_paths)?;
    Ok(d.unbind())
}

#[pyclass(name = "Client")]
struct PyClient {
    socket_path: String,
}

#[pymethods]
impl PyClient {
    #[new]
    #[pyo3(signature = (socket_path=None))]
    fn new(socket_path: Option<String>) -> Self {
        let socket_path = socket_path.unwrap_or_else(|| {
            simgit_sdk::client::default_socket_path()
                .to_string_lossy()
                .to_string()
        });
        Self { socket_path }
    }

    #[getter]
    fn socket_path(&self) -> String {
        self.socket_path.clone()
    }

    #[pyo3(signature = (task_id, agent_label=None, base_commit=None, peers=false))]
    fn session_new(
        &self,
        py: Python<'_>,
        task_id: String,
        agent_label: Option<String>,
        base_commit: Option<String>,
        peers: bool,
    ) -> PyResult<PySession> {
        let client = Client::new(&self.socket_path);
        let info = run_async(client.session_create(task_id, agent_label, base_commit, peers))?;
        let cached_info = session_info_dict(py, &info)?;
        Ok(PySession {
            socket_path: self.socket_path.clone(),
            session_id: info.session_id.to_string(),
            task_id: info.task_id.clone(),
            cached_info: Some(cached_info),
        })
    }
}

#[pyclass(name = "Session")]
struct PySession {
    socket_path: String,
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    task_id: String,
    cached_info: Option<Py<PyDict>>,
}

#[pymethods]
impl PySession {
    #[staticmethod]
    #[pyo3(signature = (task_id, socket_path=None, agent_label=None, base_commit=None, peers=false))]
    fn new(
        py: Python<'_>,
        task_id: String,
        socket_path: Option<String>,
        agent_label: Option<String>,
        base_commit: Option<String>,
        peers: bool,
    ) -> PyResult<Self> {
        let client = PyClient::new(socket_path);
        client.session_new(py, task_id, agent_label, base_commit, peers)
    }

    fn info(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        if let Some(info) = &self.cached_info {
            return Ok(info.clone_ref(py));
        }
        let client = Client::new(&self.socket_path);
        let session_id = Uuid::parse_str(&self.session_id)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let sessions = run_async(client.session_list(None))?;
        let info = sessions
            .into_iter()
            .find(|s| s.session_id == session_id)
            .ok_or_else(|| PyRuntimeError::new_err("session not found"))?;
        session_info_dict(py, &info)
    }

    #[pyo3(signature = (branch_name=None, message=None))]
    fn commit(
        &mut self,
        py: Python<'_>,
        branch_name: Option<String>,
        message: Option<String>,
    ) -> PyResult<Py<PyDict>> {
        let client = Client::new(&self.socket_path);
        let session_id = Uuid::parse_str(&self.session_id)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let info = run_async(client.session_commit(session_id, branch_name, message))?;
        let d = session_info_dict(py, &info)?;
        self.cached_info = Some(d.clone_ref(py));
        Ok(d)
    }

    fn abort(&self) -> PyResult<()> {
        let client = Client::new(&self.socket_path);
        let session_id = Uuid::parse_str(&self.session_id)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        run_async(client.session_abort(session_id))
    }

    fn diff(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        let client = Client::new(&self.socket_path);
        let session_id = Uuid::parse_str(&self.session_id)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let diff = run_async(client.session_diff(session_id))?;
        diff_result_dict(py, &diff)
    }
}

#[pyfunction]
fn default_socket_path() -> String {
    simgit_sdk::client::default_socket_path()
        .to_string_lossy()
        .to_string()
}

#[pymodule]
fn simgit(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyClient>()?;
    m.add_class::<PySession>()?;
    m.add_function(wrap_pyfunction!(default_socket_path, m)?)?;
    Ok(())
}