pub(crate) mod appearance;
pub(crate) mod apps;
pub(crate) mod editors;
pub(crate) mod network;
pub(crate) mod projects;
pub(crate) mod settings;
pub(crate) mod tasks;
pub(crate) mod terminal;

use tauri::State;

use crate::cage::manager::Manager;
use crate::runtime::Microsandbox;

pub(crate) type Projects<'a> = State<'a, Manager<Microsandbox>>;
