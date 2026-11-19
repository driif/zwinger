pub(crate) mod appearance;
pub(crate) mod cages;
pub(crate) mod editors;
pub(crate) mod network;
pub(crate) mod sessions;
pub(crate) mod settings;
pub(crate) mod tasks;
pub(crate) mod terminal;
pub(crate) mod window;

use tauri::State;

use crate::cage::manager::Manager;
use crate::runtime::Microsandbox;

pub(crate) type Managed<'a> = State<'a, Manager<Microsandbox>>;
