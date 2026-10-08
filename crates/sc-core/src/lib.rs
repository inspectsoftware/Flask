//! Data collection and system actions for SysCentral. No UI code lives here.

pub mod actions;
pub mod apps;
pub mod autoruns;
pub mod baseline;
pub mod gpu;
pub mod locks;
pub mod net;
pub mod origin;
pub mod pdh;
pub mod perf;
pub mod power;
pub mod process;
pub mod procinfo;
pub mod reg;
pub mod services;
pub mod sha256;
pub mod specs;
pub mod system;

mod nt;

pub use windows::core::Error;
pub type Result<T> = windows::core::Result<T>;
