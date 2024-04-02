#[cfg(not(target_os = "linux"))]
compile_error!("mirrord-overseer supports only linux");

pub mod fd_share;
pub mod notifier;
