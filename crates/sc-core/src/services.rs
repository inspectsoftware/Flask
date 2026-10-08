//! Windows services: list, start, stop and change how they start.

use windows::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, ENUM_SERVICE_STATUS_PROCESSW, ENUM_SERVICE_TYPE,
    EnumServicesStatusExW, OpenSCManagerW, OpenServiceW, SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_ENUMERATE_SERVICE, SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_CONTROL_STOP,
    SERVICE_DEMAND_START, SERVICE_DISABLED, SERVICE_ERROR, SERVICE_START, SERVICE_START_TYPE, SERVICE_STATE_ALL,
    SERVICE_STATUS, SERVICE_STOP, SERVICE_WIN32, StartServiceW,
};
use windows::core::PCWSTR;

use crate::reg::{Hive, Key, View};

const SERVICE_NO_CHANGE: u32 = 0xFFFF_FFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Running,
    Starting,
    Stopping,
    Paused,
    Stopped,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Running => "Running",
            State::Starting => "Starting",
            State::Stopping => "Stopping",
            State::Paused => "Paused",
            State::Stopped => "Stopped",
        }
    }

    /// From the SCM's `dwCurrentState`.
    pub fn from_code(code: u32) -> State {
        match code {
            4 => State::Running,
            2 => State::Starting,
            3 => State::Stopping,
            5..=7 => State::Paused,
            _ => State::Stopped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StartType {
    Automatic,
    Manual,
    Disabled,
    /// Boot or system start; only drivers use these.
    Other,
}

impl StartType {
    pub const CHOICES: [StartType; 3] = [StartType::Automatic, StartType::Manual, StartType::Disabled];

    pub fn label(self) -> &'static str {
        match self {
            StartType::Automatic => "Automatic",
            StartType::Manual => "Manual",
            StartType::Disabled => "Disabled",
            StartType::Other => "System",
        }
    }

    /// From the registry `Start` value.
    pub fn from_code(code: u32) -> StartType {
        match code {
            2 => StartType::Automatic,
            3 => StartType::Manual,
            4 => StartType::Disabled,
            _ => StartType::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Short name used by the service manager.
    pub name: String,
    pub display: String,
    pub state: State,
    /// 0 when not running.
    pub pid: u32,
    pub start: StartType,
}

struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Every Win32 service with its state and start type.
pub fn list() -> Vec<Service> {
    let mut out = Vec::new();
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_ENUMERATE_SERVICE) else { return out };
        let scm = Handle(scm);
        let (mut needed, mut count, mut resume) = (0u32, 0u32, 0u32);
        let kind: ENUM_SERVICE_TYPE = SERVICE_WIN32;
        let _ = EnumServicesStatusExW(
            scm.0,
            SC_ENUM_PROCESS_INFO,
            kind,
            SERVICE_STATE_ALL,
            None,
            &mut needed,
            &mut count,
            Some(&mut resume),
            None,
        );
        let mut buf = vec![0u64; (needed as usize).div_ceil(8) + 256];
        resume = 0;
        let bytes = std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len() * 8);
        if EnumServicesStatusExW(
            scm.0,
            SC_ENUM_PROCESS_INFO,
            kind,
            SERVICE_STATE_ALL,
            Some(bytes),
            &mut needed,
            &mut count,
            Some(&mut resume),
            None,
        )
        .is_err()
        {
            return out;
        }
        let items = std::slice::from_raw_parts(buf.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(), count as usize);
        let services = Key::open(Hive::Hklm, r"SYSTEM\CurrentControlSet\Services", View::Native);
        for s in items {
            let (Ok(name), Ok(display)) = (s.lpServiceName.to_string(), s.lpDisplayName.to_string()) else { continue };
            // The start type comes from the registry: one read instead of
            // opening every service.
            let start = Key::open(Hive::Hklm, &format!(r"SYSTEM\CurrentControlSet\Services\{name}"), View::Native)
                .and_then(|k| k.dword("Start"))
                .map_or(StartType::Other, StartType::from_code);
            out.push(Service {
                name,
                display,
                state: State::from_code(s.ServiceStatusProcess.dwCurrentState.0),
                pid: s.ServiceStatusProcess.dwProcessId,
                start,
            });
        }
        drop(services);
    }
    out
}

fn open(name: &str, access: u32) -> crate::Result<(Handle, Handle)> {
    let wname = wide(name);
    unsafe {
        let scm = Handle(OpenSCManagerW(None, None, SC_MANAGER_CONNECT)?);
        let service = Handle(OpenServiceW(scm.0, PCWSTR(wname.as_ptr()), access)?);
        Ok((scm, service))
    }
}

/// Asks the service to start. Returns once the request is accepted, not when
/// the service is running.
pub fn start(name: &str) -> crate::Result<()> {
    let (_scm, service) = open(name, SERVICE_START)?;
    unsafe { StartServiceW(service.0, None) }
}

/// Asks the service to stop.
pub fn stop(name: &str) -> crate::Result<()> {
    let (_scm, service) = open(name, SERVICE_STOP)?;
    let mut status = SERVICE_STATUS::default();
    unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) }
}

pub fn set_start(name: &str, start: StartType) -> crate::Result<()> {
    let code: SERVICE_START_TYPE = match start {
        StartType::Automatic => SERVICE_AUTO_START,
        StartType::Manual => SERVICE_DEMAND_START,
        StartType::Disabled => SERVICE_DISABLED,
        StartType::Other => return Ok(()),
    };
    let (_scm, service) = open(name, SERVICE_CHANGE_CONFIG)?;
    unsafe {
        ChangeServiceConfigW(
            service.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            code,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_mappings() {
        assert_eq!(State::from_code(4), State::Running);
        assert_eq!(State::from_code(1), State::Stopped);
        assert_eq!(State::from_code(7), State::Paused);
        assert_eq!(StartType::from_code(2), StartType::Automatic);
        assert_eq!(StartType::from_code(3), StartType::Manual);
        assert_eq!(StartType::from_code(4), StartType::Disabled);
        assert_eq!(StartType::from_code(0), StartType::Other);
    }

    #[test]
    fn listing_includes_core_services() {
        let services = list();
        assert!(services.len() > 50, "{}", services.len());
        let rpc = services.iter().find(|s| s.name.eq_ignore_ascii_case("RpcSs")).expect("RPC service");
        assert_eq!(rpc.state, State::Running);
        assert!(rpc.pid != 0 && rpc.start == StartType::Automatic);
        assert!(services.iter().any(|s| s.state == State::Stopped && s.pid == 0));
    }

    #[test]
    fn unknown_service_reports_an_error() {
        assert!(start("sc-core-no-such-service").is_err());
        assert!(stop("sc-core-no-such-service").is_err());
        assert!(set_start("sc-core-no-such-service", StartType::Manual).is_err());
    }
}
