use std::{
    collections::HashMap,
    iter::FromIterator,
    sync::{Arc, Mutex},
};

use sciter::Value;

use hbb_common::{
    allow_err,
    config::{LocalConfig, PeerConfig},
    log,
};

#[cfg(not(any(feature = "flutter", feature = "cli")))]
use crate::ui_session_interface::Session;
use crate::{common::get_app_name, ipc, ui_interface::*};

mod cm;
#[cfg(feature = "inline")]
pub mod inline;
pub mod remote;

#[allow(dead_code)]
type Status = (i32, bool, i64, String);

lazy_static::lazy_static! {
    // stupid workaround for https://sciter.com/forums/topic/crash-on-latest-tis-mac-sdk-sometimes/
    static ref STUPID_VALUES: Mutex<Vec<Arc<Vec<Value>>>> = Default::default();
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
lazy_static::lazy_static! {
    pub static ref CUR_SESSION: Arc<Mutex<Option<Session<remote::SciterHandler>>>> = Default::default();
}

struct UIHostHandler;

pub fn start(args: &mut [String]) {
    #[cfg(target_os = "macos")]
    crate::platform::delegate::show_dock();
    #[cfg(all(target_os = "linux", feature = "inline"))]
    {
        let app_dir = std::env::var("APPDIR").unwrap_or("".to_string());
        let mut so_path = "/usr/lib/rustdesk/libsciter-gtk.so".to_owned();
        for (prefix, dir) in [
            ("", "/usr"),
            ("", "/app"),
            (&app_dir, "/usr"),
            (&app_dir, "/app"),
        ]
        .iter()
        {
            let path = format!("{prefix}{dir}/lib/rustdesk/libsciter-gtk.so");
            if std::path::Path::new(&path).exists() {
                so_path = path;
                break;
            }
        }
        sciter::set_library(&so_path).ok();
    }
    #[cfg(windows)]
    // Check if there is a sciter.dll nearby.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sciter_dll_path = parent.join("sciter.dll");
            if sciter_dll_path.exists() {
                // Try to set the sciter dll.
                let p = sciter_dll_path.to_string_lossy().to_string();
                log::debug!("Found dll:{}, \n {:?}", p, sciter::set_library(&p));
            }
        }
    }
    // https://github.com/c-smile/sciter-sdk/blob/master/include/sciter-x-types.h
    // https://github.com/rustdesk/rustdesk/issues/132#issuecomment-886069737
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::GfxLayer(
        sciter::GFX_LAYER::WARP
    )));
    use sciter::SCRIPT_RUNTIME_FEATURES::*;
    allow_err!(sciter::set_options(sciter::RuntimeOptions::ScriptFeatures(
        ALLOW_FILE_IO as u8 | ALLOW_SOCKET_IO as u8 | ALLOW_EVAL as u8 | ALLOW_SYSINFO as u8
    )));
    let mut frame = sciter::WindowBuilder::main_window().create();
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::UxTheming(true)));
    frame.set_title(&crate::get_app_name());
    #[cfg(target_os = "macos")]
    crate::platform::delegate::make_menubar(frame.get_host(), args.is_empty());
    #[cfg(windows)]
    crate::platform::try_set_window_foreground(frame.get_hwnd() as _);
    let page;
    if args.len() > 1 && args[0] == "--play" {
        args[0] = "--connect".to_owned();
        let path: std::path::PathBuf = (&args[1]).into();
        let id = path
            .file_stem()
            .map(|p| p.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_owned();
        args[1] = id;
    }
    if args.is_empty() {
        std::thread::spawn(move || check_zombie());
        crate::common::check_software_update();
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "index.html";
        // Start pulse audio local server.
        #[cfg(target_os = "linux")]
        std::thread::spawn(crate::ipc::start_pa);
    } else if args[0] == "--install" {
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "install.html";
    } else if args[0] == "--cm" {
        frame.register_behavior("connection-manager", move || {
            Box::new(cm::SciterConnectionManager::new())
        });
        page = "cm.html";
    } else if (args[0] == "--connect"
        || args[0] == "--file-transfer"
        || args[0] == "--port-forward"
        || args[0] == "--rdp")
        && args.len() > 1
    {
        #[cfg(windows)]
        {
            let hw = frame.get_host().get_hwnd();
            crate::platform::windows::enable_lowlevel_keyboard(hw as _);
        }
        let mut iter = args.iter();
        let Some(cmd) = iter.next() else {
            log::error!("Failed to get cmd arg");
            return;
        };
        let cmd = cmd.to_owned();
        let Some(id) = iter.next() else {
            log::error!("Failed to get id arg");
            return;
        };
        let id = id.to_owned();
        let pass = iter.next().unwrap_or(&"".to_owned()).clone();
        let args: Vec<String> = iter.map(|x| x.clone()).collect();
        frame.set_title(&id);
        frame.register_behavior("native-remote", move || {
            let handler =
                remote::SciterSession::new(cmd.clone(), id.clone(), pass.clone(), args.clone());
            #[cfg(not(any(feature = "flutter", feature = "cli")))]
            {
                *CUR_SESSION.lock().unwrap() = Some(handler.inner());
            }
            Box::new(handler)
        });
        page = "remote.html";
    } else {
        log::error!("Wrong command: {:?}", args);
        return;
    }
    #[cfg(feature = "inline")]
    {
        let html = if page == "index.html" {
            inline::get_index()
        } else if page == "cm.html" {
            inline::get_cm()
        } else if page == "install.html" {
            inline::get_install()
        } else {
            inline::get_remote()
        };
        frame.load_html(html.as_bytes(), Some(page));
    }
    #[cfg(not(feature = "inline"))]
    frame.load_file(&format!(
        "file://{}/src/ui/{}",
        std::env::current_dir()
            .map(|c| c.display().to_string())
            .unwrap_or("".to_owned()),
        page
    ));
    frame.run_app();
}

struct UI {}

impl UI {
    fn recent_sessions_updated(&self) -> bool {
        recent_sessions_updated()
    }

    fn get_id(&self) -> String {
        ipc::get_id()
    }

    fn temporary_password(&mut self) -> String {
        temporary_password()
    }

    fn update_temporary_password(&self) {
        update_temporary_password()
    }

    fn permanent_password(&self) -> String {
        permanent_password()
    }

    fn set_permanent_password(&self, password: String) {
        set_permanent_password(password);
    }

    fn get_remote_id(&mut self) -> String {
        LocalConfig::get_remote_id()
    }

    fn set_remote_id(&mut self, id: String) {
        LocalConfig::set_remote_id(&id);
    }

    fn goto_install(&mut self) {
        goto_install();
    }

    fn install_me(&mut self, _options: String, _path: String) {
        install_me(_options, _path, false, false);
    }

    fn update_me(&self, _path: String) {
        update_me(_path);
    }

    fn run_without_install(&self) {
        run_without_install();
    }

    fn show_run_without_install(&self) -> bool {
        show_run_without_install()
    }

    fn get_license(&self) -> String {
        get_license()
    }

    fn get_option(&self, key: String) -> String {
        get_option(key)
    }

    fn get_local_option(&self, key: String) -> String {
        get_local_option(key)
    }

    fn set_local_option(&self, key: String, value: String) {
        set_local_option(key, value);
    }

    fn peer_has_password(&self, id: String) -> bool {
        peer_has_password(id)
    }

    fn forget_password(&self, id: String) {
        forget_password(id)
    }

    fn get_peer_option(&self, id: String, name: String) -> String {
        get_peer_option(id, name)
    }

    fn set_peer_option(&self, id: String, name: String, value: String) {
        set_peer_option(id, name, value)
    }

    fn using_public_server(&self) -> bool {
        crate::using_public_server()
    }

    fn get_options(&self) -> Value {
        let hashmap: HashMap<String, String> =
            serde_json::from_str(&get_options()).unwrap_or_default();
        let mut m = Value::map();
        for (k, v) in hashmap {
            m.set_item(k, v);
        }
        m
    }

    fn test_if_valid_server(&self, host: String, test_with_proxy: bool) -> String {
        test_if_valid_server(host, test_with_proxy)
    }

    fn get_sound_inputs(&self) -> Value {
        Value::from_iter(get_sound_inputs())
    }

    fn set_options(&self, v: Value) {
        let mut m = HashMap::new();
        for (k, v) in v.items() {
            if let Some(k) = k.as_string() {
                if let Some(v) = v.as_string() {
                    if !v.is_empty() {
                        m.insert(k, v);
                    }
                }
            }
        }
        set_options(m);
    }

    fn set_option(&self, key: String, value: String) {
        set_option(key, value);
    }

    fn install_path(&mut self) -> String {
        install_path()
    }

    fn get_socks(&self) -> Value {
        Value::from_iter(get_socks())
    }

    fn set_socks(&self, proxy: String, username: String, password: String) {
        set_socks(proxy, username, password)
    }

    fn is_installed(&self) -> bool {
        is_installed()
    }

    fn is_root(&self) -> bool {
        is_root()
    }

    fn is_release(&self) -> bool {
        #[cfg(not(debug_assertions))]
        return true;
        #[cfg(debug_assertions)]
        return false;
    }

    fn is_share_rdp(&self) -> bool {
        is_share_rdp()
    }

    fn set_share_rdp(&self, _enable: bool) {
        set_share_rdp(_enable);
    }

    fn is_installed_lower_version(&self) -> bool {
        is_installed_lower_version()
    }

    fn closing(&mut self, x: i32, y: i32, w: i32, h: i32) {
        crate::server::input_service::fix_key_down_timeout_at_exit();
        LocalConfig::set_size(x, y, w, h);
    }

    fn get_size(&mut self) -> Value {
        let s = LocalConfig::get_size();
        let mut v = Vec::new();
        v.push(s.0);
        v.push(s.1);
        v.push(s.2);
        v.push(s.3);
        Value::from_iter(v)
    }

    fn get_mouse_time(&self) -> f64 {
        get_mouse_time()
    }

    fn check_mouse_time(&self) {
        check_mouse_time()
    }

    fn get_connect_status(&mut self) -> Value {
        let mut v = Value::array(0);
        let x = get_connect_status();
        v.push(x.status_num);
        v.push(x.key_confirmed);
        v.push(x.id);
        v
    }

    #[inline]
    fn get_peer_value(id: String, p: PeerConfig) -> Value {
        let values = vec![
            id,
            p.info.username.clone(),
            p.info.hostname.clone(),
            p.info.platform.clone(),
            p.options.get("alias").unwrap_or(&"".to_owned()).to_owned(),
        ];
        Value::from_iter(values)
    }

    fn get_peer(&self, id: String) -> Value {
        let c = get_peer(id.clone());
        Self::get_peer_value(id, c)
    }

    fn get_fav(&self) -> Value {
        Value::from_iter(get_fav())
    }

    fn store_fav(&self, fav: Value) {
        let mut tmp = vec![];
        fav.values().for_each(|v| {
            if let Some(v) = v.as_string() {
                if !v.is_empty() {
                    tmp.push(v);
                }
            }
        });
        store_fav(tmp);
    }

    fn get_recent_sessions(&mut self) -> Value {
        // to-do: limit number of recent sessions, and remove old peer file
        let peers: Vec<Value> = PeerConfig::peers(None)
            .drain(..)
            .map(|p| Self::get_peer_value(p.0, p.2))
            .collect();
        Value::from_iter(peers)
    }

    fn get_icon(&mut self) -> String {
        get_icon()
    }

    fn remove_peer(&mut self, id: String) {
        PeerConfig::remove(&id);
    }

    fn remove_discovered(&mut self, id: String) {
        remove_discovered(id);
    }

    fn send_wol(&mut self, id: String) {
        crate::lan::send_wol(id)
    }

    fn new_remote(&mut self, id: String, remote_type: String, force_relay: bool) {
        new_remote(id, remote_type, force_relay)
    }

    fn is_process_trusted(&mut self, _prompt: bool) -> bool {
        is_process_trusted(_prompt)
    }

    fn is_can_screen_recording(&mut self, _prompt: bool) -> bool {
        is_can_screen_recording(_prompt)
    }

    fn is_installed_daemon(&mut self, _prompt: bool) -> bool {
        is_installed_daemon(_prompt)
    }

    fn get_error(&mut self) -> String {
        get_error()
    }

    fn is_login_wayland(&mut self) -> bool {
        is_login_wayland()
    }

    fn current_is_wayland(&mut self) -> bool {
        current_is_wayland()
    }

    fn get_software_update_url(&self) -> String {
        crate::SOFTWARE_UPDATE_URL.lock().unwrap().clone()
    }

    fn get_new_version(&self) -> String {
        get_new_version()
    }

    fn get_version(&self) -> String {
        get_version()
    }

    fn get_fingerprint(&self) -> String {
        get_fingerprint()
    }

    fn get_app_name(&self) -> String {
        get_app_name()
    }

    fn get_software_ext(&self) -> String {
        #[cfg(windows)]
        let p = "exe";
        #[cfg(target_os = "macos")]
        let p = "dmg";
        #[cfg(target_os = "linux")]
        let p = "deb";
        p.to_owned()
    }

    fn get_software_store_path(&self) -> String {
        let mut p = std::env::temp_dir();
        let name = crate::SOFTWARE_UPDATE_URL
            .lock()
            .unwrap()
            .split("/")
            .last()
            .map(|x| x.to_owned())
            .unwrap_or(crate::get_app_name());
        p.push(name);
        format!("{}.{}", p.to_string_lossy(), self.get_software_ext())
    }

    fn create_shortcut(&self, _id: String) {
        #[cfg(windows)]
        create_shortcut(_id)
    }

    fn discover(&self) {
        std::thread::spawn(move || {
            allow_err!(crate::lan::discover());
        });
    }

    fn get_lan_peers(&self) -> String {
        // let peers = get_lan_peers()
        //     .into_iter()
        //     .map(|mut peer| {
        //         (
        //             peer.remove("id").unwrap_or_default(),
        //             peer.remove("username").unwrap_or_default(),
        //             peer.remove("hostname").unwrap_or_default(),
        //             peer.remove("platform").unwrap_or_default(),
        //         )
        //     })
        //     .collect::<Vec<(String, String, String, String)>>();
        serde_json::to_string(&get_lan_peers()).unwrap_or_default()
    }

    fn get_uuid(&self) -> String {
        get_uuid()
    }

    fn open_url(&self, url: String) {
        #[cfg(windows)]
        let p = "explorer";
        #[cfg(target_os = "macos")]
        let p = "open";
        #[cfg(target_os = "linux")]
        let p = if std::path::Path::new("/usr/bin/firefox").exists() {
            "firefox"
        } else {
            "xdg-open"
        };
        allow_err!(std::process::Command::new(p).arg(url).spawn());
    }

    fn change_id(&self, id: String) {
        reset_async_job_status();
        let old_id = self.get_id();
        change_id_shared(id, old_id);
    }

    fn http_request(&self, url: String, method: String, body: Option<String>, header: String) {
        http_request(url, method, body, header)
    }

    fn post_request(&self, url: String, body: String, header: String) {
        post_request(url, body, header)
    }

    fn is_ok_change_id(&self) -> bool {
        hbb_common::machine_uid::get().is_ok()
    }

    fn get_async_job_status(&self) -> String {
        get_async_job_status()
    }

    fn get_http_status(&self, url: String) -> Option<String> {
        get_async_http_status(url)
    }

    fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    fn is_xfce(&self) -> bool {
        crate::platform::is_xfce()
    }

    fn get_api_server(&self) -> String {
        get_api_server()
    }

    fn has_hwcodec(&self) -> bool {
        has_hwcodec()
    }

    fn has_vram(&self) -> bool {
        has_vram()
    }

    fn get_langs(&self) -> String {
        get_langs()
    }

    fn video_save_directory(&self, root: bool) -> String {
        video_save_directory(root)
    }

    fn handle_relay_id(&self, id: String) -> String {
        handle_relay_id(&id).to_owned()
    }

    fn get_login_device_info(&self) -> String {
        get_login_device_info_json()
    }

    fn support_remove_wallpaper(&self) -> bool {
        support_remove_wallpaper()
    }

    fn has_valid_2fa(&self) -> bool {
        has_valid_2fa()
    }

    fn generate2fa(&self) -> String {
        generate2fa()
    }

    pub fn verify2fa(&self, code: String) -> bool {
        verify2fa(code)
    }

    fn generate_2fa_img_src(&self, data: String) -> String {
        let v = qrcode_generator::to_png_to_vec(data, qrcode_generator::QrCodeEcc::Low, 128)
            .unwrap_or_default();
        let s = hbb_common::sodiumoxide::base64::encode(
            v,
            hbb_common::sodiumoxide::base64::Variant::Original,
        );
        format!("data:image/png;base64,{s}")
    }

    pub fn check_hwcodec(&self) {
        check_hwcodec()
    }
}

impl sciter::EventHandler for UI {
    sciter::dispatch_script_call! {
        fn t(String);
        fn get_api_server();
        fn is_xfce();
        fn using_public_server();
        fn get_id();
        fn temporary_password();
        fn update_temporary_password();
        fn permanent_password();
        fn set_permanent_password(String);
        fn get_remote_id();
        fn set_remote_id(String);
        fn closing(i32, i32, i32, i32);
        fn get_size();
        fn new_remote(String, String, bool);
        fn send_wol(String);
        fn remove_peer(String);
        fn remove_discovered(String);
        fn get_connect_status();
        fn get_mouse_time();
        fn check_mouse_time();
        fn get_recent_sessions();
        fn get_peer(String);
        fn get_fav();
        fn store_fav(Value);
        fn recent_sessions_updated();
        fn get_icon();
        fn install_me(String, String);
        fn is_installed();
        fn is_root();
        fn is_release();
        fn set_socks(String, String, String);
        fn get_socks();
        fn is_share_rdp();
        fn set_share_rdp(bool);
        fn is_installed_lower_version();
        fn install_path();
        fn goto_install();
        fn is_process_trusted(bool);
        fn is_can_screen_recording(bool);
        fn is_installed_daemon(bool);
        fn get_error();
        fn is_login_wayland();
        fn current_is_wayland();
        fn get_options();
        fn get_option(String);
        fn get_local_option(String);
        fn set_local_option(String, String);
        fn get_peer_option(String, String);
        fn peer_has_password(String);
        fn forget_password(String);
        fn set_peer_option(String, String, String);
        fn get_license();
        fn test_if_valid_server(String, bool);
        fn get_sound_inputs();
        fn set_options(Value);
        fn set_option(String, String);
        fn get_software_update_url();
        fn get_new_version();
        fn get_version();
        fn get_fingerprint();
        fn update_me(String);
        fn show_run_without_install();
        fn run_without_install();
        fn get_app_name();
        fn get_software_store_path();
        fn get_software_ext();
        fn open_url(String);
        fn change_id(String);
        fn get_async_job_status();
        fn post_request(String, String, String);
        fn is_ok_change_id();
        fn create_shortcut(String);
        fn discover();
        fn get_lan_peers();
        fn get_uuid();
        fn has_hwcodec();
        fn has_vram();
        fn get_langs();
        fn video_save_directory(bool);
        fn handle_relay_id(String);
        fn get_login_device_info();
        fn support_remove_wallpaper();
        fn has_valid_2fa();
        fn generate2fa();
        fn generate_2fa_img_src(String);
        fn verify2fa(String);
        fn check_hwcodec();
    }
}

impl sciter::host::HostHandler for UIHostHandler {
    fn on_graphics_critical_failure(&mut self) {
        log::error!("Critical rendering error: e.g. DirectX gfx driver error. Most probably bad gfx drivers.");
    }
}

#[cfg(not(target_os = "linux"))]
fn get_sound_inputs() -> Vec<String> {
    let mut out = Vec::new();
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if let Ok(devices) = host.devices() {
        for device in devices {
            if device.default_input_config().is_err() {
                continue;
            }
            if let Ok(name) = device.name() {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn get_sound_inputs() -> Vec<String> {
    crate::platform::linux::get_pa_sources()
        .drain(..)
        .map(|x| x.1)
        .collect()
}

// sacrifice some memory
pub fn value_crash_workaround(values: &[Value]) -> Arc<Vec<Value>> {
    let persist = Arc::new(values.to_vec());
    STUPID_VALUES.lock().unwrap().push(persist.clone());
    persist
}

pub fn get_icon() -> String {
    // 128x128
    #[cfg(target_os = "macos")]
    // 128x128 on 160x160 canvas, then shrink to 128, mac looks better with padding
    {
        "data:image/png;base64,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".into()
    }
    #[cfg(not(target_os = "macos"))] // 128x128 no padding
    {
        "data:image/png;base64,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".into()
    }
}
