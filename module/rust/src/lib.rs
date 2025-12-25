mod api;
mod binding;
#[doc(hidden)]
pub mod macros;
mod module;

#[macro_use]
extern crate log;
#[cfg(target_os = "android")]
extern crate android_logger;

#[cfg(target_os = "android")]
use android_logger::Config;
#[cfg(target_os = "android")]
use log::LevelFilter;

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::OnceLock;

pub use api::ZygiskApi;
pub use binding::{AppSpecializeArgs, ServerSpecializeArgs, StateFlags, ZygiskOption, API_VERSION};
use jni::{JNIEnv, JavaVM};
pub use module::ZygiskModule;

// config & payload path
const CONFIG_PATH: &str = "/data/adb/modules/zygisk-loader/active_config.txt";
const PAYLOAD_FILENAME: &str = "payload.so";

static MODULE: ZygiskLoaderModule = ZygiskLoaderModule {};
crate::zygisk_module!(&MODULE);

struct ZygiskLoaderModule {}

// Static variable to store JavaVM
static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

// Static variable to store config
static TARGET_CONFIG: OnceLock<String> = OnceLock::new();

// Save the File Descriptor (FD) of the payload
static PAYLOAD_FD: OnceLock<i32> = OnceLock::new();

impl ZygiskModule for ZygiskLoaderModule {
    fn on_load(&self, _api: ZygiskApi, env: JNIEnv) {
        #[cfg(target_os = "android")]
        android_logger::init_once(
            Config::default()
                .with_max_level(LevelFilter::Debug)
                .with_tag("Zygisk_Loader"),
        );

        let vm = env.get_java_vm().expect("Failed to get JavaVM");
        let _ = JAVA_VM.set(vm);
        info!("Zygisk-Loader Loaded.");
    }

    fn pre_app_specialize(&self, api: ZygiskApi, args: &mut AppSpecializeArgs) {
        // 1. Read Config
        if let Ok(target) = read_target_config() {
            let _ = TARGET_CONFIG.set(target);
        }

        // 2. Prepare Payload via FD (File Descriptor)
        let current_process = get_process_name_from_args_safe(args);
        let target_package = TARGET_CONFIG.get().map(|s| s.as_str()).unwrap_or("");

        if !target_package.is_empty() && current_process.contains(target_package) {
            info!("Target '{}'. Preparing FD access...", current_process);

            // Use Zygisk API to get FD to module folder (we have root access here)
            let module_dir_fd = api.get_module_dir();

            if module_dir_fd >= 0 {
                // Open the payload.so file using 'openat' (relative to dir FD)
                // We have to be unsafe because we are calling the C library directly.
                let c_filename = CString::new(PAYLOAD_FILENAME).unwrap();

                let payload_fd = unsafe {
                    libc::openat(
                        module_dir_fd,
                        c_filename.as_ptr(),
                        libc::O_RDONLY,
                        0
                    )
                };

                if payload_fd >= 0 {
                    info!("Payload FD opened: {}", payload_fd);

                    // Tell Zygisk to close this FD when it's finished
                    // BUT don't close it now, exclude it from automatic closing during specialization
                    api.exempt_fd(payload_fd);

                    let _ = PAYLOAD_FD.set(payload_fd);
                } else {
                    error!("Failed to open payload.so via FD. File exists? Errno: {}", payload_fd);
                }
            } else {
                error!("Failed to get module dir FD");
            }
        }
    }

    fn post_app_specialize(&self, _api: ZygiskApi, args: &AppSpecializeArgs) {
        let current_process = get_process_name_from_args_safe(args);
        let target_package = TARGET_CONFIG.get().map(|s| s.as_str()).unwrap_or("");

        if !target_package.is_empty() && current_process.contains(target_package) {
            if let Some(fd) = PAYLOAD_FD.get() {
                // We will do a dlopen using the path /proc/self/fd/{fd}
                // This allows us to load files without namespace-blocked absolute paths.
                let path = format!("/proc/self/fd/{}", fd);
                info!("Injecting via FD path: {}", path);

                unsafe {
                    inject_payload(&path);
                }
            } else {
                info!("No FD found. Skipping injection.");
            }
        }
    }
}

// Helper: Read Config
fn read_target_config() -> std::io::Result<String> {
    let f = File::open(CONFIG_PATH)?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim().to_string())
}

// Helper: dlopen standard
unsafe fn inject_payload(path: &str) {
    let c_path = CString::new(path).unwrap();
    let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW);

    if handle.is_null() {
        let err_ptr = libc::dlerror();
        let err_msg = if !err_ptr.is_null() {
            CStr::from_ptr(err_ptr).to_string_lossy().into_owned()
        } else {
            "Unknown error".to_string()
        };
        error!("dlopen failed: {}", err_msg);
    } else {
        info!("Payload loaded successfully! Handle: {:p}", handle);
    }
}

// Helper: Get App Data Dir String
fn get_app_data_dir_string(args: &AppSpecializeArgs) -> Option<String> {
    if let Some(vm) = JAVA_VM.get() {
        if let Ok(env) = vm.attach_current_thread_as_daemon() {
            let jstring = *args.app_data_dir;
            if let Ok(s) = env.get_string(jstring) {
                return Some(s.into());
            }
        }
    }
    None
}

// Helper: Get Process Name
fn get_process_name_from_args_safe(args: &AppSpecializeArgs) -> String {
    if let Some(vm) = JAVA_VM.get() {
        match vm.attach_current_thread_as_daemon() {
            Ok(env) => {
                let nice_name_jstring = *args.nice_name;
                if let Ok(nice_name_str) = env.get_string(nice_name_jstring) {
                    let nice_name_rust: String = nice_name_str.into();
                    if !nice_name_rust.is_empty() {
                        return nice_name_rust;
                    }
                }
                let app_data_dir_jstring = *args.app_data_dir;
                if let Ok(app_data_dir_str) = env.get_string(app_data_dir_jstring) {
                    let app_data_dir_rust: String = app_data_dir_str.into();
                    if !app_data_dir_rust.is_empty() {
                        return extract_package_from_path(&app_data_dir_rust);
                    }
                }
            },
            Err(e) => {
                error!("Failed to attach JVM: {:?}", e);
            }
        }
    }
    String::new()
}

// Helper: Extract Package Name
fn extract_package_from_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 3 {
        for part in parts.iter().rev() {
            if !part.is_empty() {
                return part.to_string();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod test {
    use std::os::unix::io::RawFd;
    fn companion(_socket: RawFd) {}
    crate::zygisk_companion!(companion);
}
