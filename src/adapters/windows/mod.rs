mod clipboard;
mod device;
pub(crate) mod dpi;
mod game;
mod input;
mod window;

pub(crate) use device::WindowsUiDevice;
pub(crate) use game::resolve_game_executable;
pub(crate) use input::parse_key;
