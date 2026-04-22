//! 安装：HKCU 自启 / Windows 服务。

#![cfg(windows)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::paths::{AppPaths, InstallScope};
use crate::{RUN_REG_VALUE, SERVICE_NAME};

#[derive(Debug, Args)]
pub struct InstallArgs {
    #[arg(value_enum)]
    pub mode: Mode,
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    #[arg(value_enum)]
    pub mode: Mode,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum Mode {
    /// HKCU\Run 自启动（普通权限，登录时启动）。
    Autostart,
    /// Windows 服务（需管理员权限）。
    Service,
}

pub fn install(args: InstallArgs) -> std::io::Result<()> {
    match args.mode {
        Mode::Autostart => install_autostart(),
        Mode::Service => install_service(),
    }
}

pub fn uninstall(args: UninstallArgs) -> std::io::Result<()> {
    match args.mode {
        Mode::Autostart => uninstall_autostart(),
        Mode::Service => uninstall_service(),
    }
}

// ── HKCU autostart ─────────────────────────────────────────────────────

fn install_autostart() -> std::io::Result<()> {
    let paths = AppPaths::for_scope(InstallScope::User)?;
    paths.ensure_dirs()?;
    let target = copy_self_to(&paths.bin_dir.join("tracker.exe"))?;
    let cmd = format!("\"{}\"", target.display());
    set_run_value(RUN_REG_VALUE, &cmd)?;
    println!("Installed HKCU autostart:");
    println!("  exe : {}", target.display());
    println!("  data: {}", paths.root.display());
    println!("(takes effect on next user logon)");
    Ok(())
}

fn uninstall_autostart() -> std::io::Result<()> {
    delete_run_value(RUN_REG_VALUE)?;
    println!("Removed HKCU autostart entry: {RUN_REG_VALUE}");
    println!("(data files preserved)");
    Ok(())
}

fn copy_self_to(dest: &Path) -> std::io::Result<PathBuf> {
    if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent)?; }
    let cur = std::env::current_exe()?;
    if cur != dest {
        match std::fs::copy(&cur, dest) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(32) /* sharing violation */ => {
                // 已在运行；尝试 .new + rename 模式不在此实现，提示用户
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("cannot overwrite {} (file in use). Stop the daemon first.", dest.display()),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(dest.to_path_buf())
}

fn set_run_value(name: &str, value: &str) -> std::io::Result<()> {
    let subkey: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0".encode_utf16().collect();
    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let r = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        );
        if r as u32 != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(r as i32));
        }
        let bytes = std::slice::from_raw_parts(
            value_w.as_ptr() as *const u8,
            value_w.len() * std::mem::size_of::<u16>(),
        );
        let r2 = RegSetValueExW(
            hkey,
            name_w.as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        );
        RegCloseKey(hkey);
        if r2 as u32 != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(r2 as i32));
        }
    }
    Ok(())
}

fn delete_run_value(name: &str) -> std::io::Result<()> {
    let subkey: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0".encode_utf16().collect();
    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let r = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        );
        if r as u32 != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(r as i32));
        }
        let r2 = RegDeleteValueW(hkey, name_w.as_ptr());
        RegCloseKey(hkey);
        if r2 as u32 != ERROR_SUCCESS && r2 as u32 != 2 /* not found */ {
            return Err(std::io::Error::from_raw_os_error(r2 as i32));
        }
    }
    Ok(())
}

// ── Windows service ────────────────────────────────────────────────────

fn install_service() -> std::io::Result<()> {
    use windows_service::{
        service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let paths = AppPaths::for_scope(InstallScope::Machine)?;
    paths.ensure_dirs()?;
    let target = copy_self_to(&paths.bin_dir.join("tracker.exe"))?;

    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
        .map_err(svc_io_err)?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("RustTimeNoter App Usage Tracker"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: target.clone(),
        launch_arguments: vec![OsString::from("service-main")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let svc = mgr
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .map_err(svc_io_err)?;
    let _ = svc.set_description("Records foreground application usage. Ultra-light, event-driven.");

    println!("Installed service '{SERVICE_NAME}'.");
    println!("  exe : {}", target.display());
    println!("  data: {}", paths.root.display());
    println!("Run: sc start {SERVICE_NAME}   (or reboot)");
    Ok(())
}

fn uninstall_service() -> std::io::Result<()> {
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(svc_io_err)?;
    let svc = mgr
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(svc_io_err)?;

    if svc.query_status().map_err(svc_io_err)?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
    }
    svc.delete().map_err(svc_io_err)?;
    println!("Removed service '{SERVICE_NAME}'.");
    Ok(())
}

fn svc_io_err(e: windows_service::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

// ── Service entry-point dispatch ───────────────────────────────────────

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    use std::sync::mpsc::{self, Sender};
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    let (stop_tx, _stop_rx) = mpsc::channel::<()>();
    let stop_tx_for_handler: Sender<()> = stop_tx.clone();
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx_for_handler.send(());
                // 让 daemon 主线程 PostQuitMessage
                unsafe {
                    use windows_sys::Win32::UI::WindowsAndMessaging::PostQuitMessage;
                    PostQuitMessage(0);
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
        Ok(h) => h,
        Err(_) => return,
    };

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    });

    // 标记 scope=machine
    std::env::set_var("RUSTTIMENOTER_SCOPE", "machine");
    let _ = crate::daemon::run(InstallScope::Machine);

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    });
    let _ = stop_tx;
}

pub fn run_service_dispatcher() -> std::io::Result<()> {
    use windows_service::service_dispatcher;
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(svc_io_err)
}
