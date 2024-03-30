#[cfg(not(target_os = "linux"))]
compile_error!("mirrord-overseet supports only linux");

pub mod fd_share;
