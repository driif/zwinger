pub(crate) mod appearance;
pub(crate) mod apps;
pub(crate) mod network;
pub(crate) mod settings;
pub(crate) mod shields;
pub(crate) mod tasks;
pub(crate) mod terminal;

use tauri::State;

use crate::runtime::Microsandbox;
use crate::shield::manager::Manager;

pub(crate) type Shields<'a> = State<'a, Manager<Microsandbox>>;
