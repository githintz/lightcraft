//! Settings ▸ Cloud driven as a person would: the passphrase typed twice (Set Up waits until
//! the two match), a folder account added from the folder picker and given a size limit, Back Up
//! Now with its result, then Lock and Unlock.

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::headless::Headless;
use crate::state::Dialog;
use crate::{LightcraftApp, Services};

const T: Duration = Duration::from_secs(20);
const PASS: &str = "correct horse battery";

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-ui-cloud-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn click(h: &mut Headless, id: &str) {
    let r = h.request("ui.clickWidget", json!({ "id": id }), T);
    assert_eq!(r["ok"], true, "{id}: {r}");
    h.step();
    h.step();
}

fn type_into(h: &mut Headless, field: &str, text: &str) {
    click(h, field);
    h.request("ui.text", json!({ "text": text }), T);
    h.step();
}

fn status(h: &mut Headless) -> Value {
    h.app.session.execute("cloud.status", &Value::Null).unwrap()
}

#[test]
fn set_up_add_a_folder_back_up_lock_and_unlock_from_settings() {
    let (lib, acc) = (temp("lib"), temp("acc"));
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.open_library(&lib, false).unwrap();
    let picked = acc.join("LightCraft").to_string_lossy().to_string();
    let services = Services { png: None, pick_folder: Some(Box::new(move || Some(picked.clone()))), ..Default::default() };
    let mut h = Headless::new(LightcraftApp::new(session, services), [1400.0, 900.0], 1.0);
    h.app.ui.dialog = Some(Dialog::Settings { tab: "cloud".into() });
    h.step();
    // the widget list switches on with the first request
    h.request("ui.widgets", json!({}), T);
    h.step();

    // a passphrase that doesn't match its repeat sets nothing up
    type_into(&mut h, "field:cloudPassphrase", PASS);
    type_into(&mut h, "field:cloudPassphraseRepeat", "correct horse");
    click(&mut h, "button:cloudSetup");
    assert_eq!(status(&mut h)["configured"], false);
    type_into(&mut h, "field:cloudPassphraseRepeat", " battery");
    click(&mut h, "button:cloudSetup");
    assert_eq!((status(&mut h)["configured"].clone(), status(&mut h)["unlocked"].clone()), (json!(true), json!(true)));

    // a folder from the picker, then a size limit so originals can go there
    h.step();
    click(&mut h, "button:cloudAddFolder");
    let accounts = status(&mut h)["accounts"].clone();
    assert_eq!(accounts.as_array().map(Vec::len), Some(1), "{accounts}");
    assert_eq!(accounts[0]["label"], "LightCraft");
    h.step();
    click(&mut h, "button:cloudCap-0-2");
    assert_eq!(status(&mut h)["accounts"][0]["settings"]["cap"], 200u64 * 1_073_741_824);

    // Back Up Now runs on a worker; the tab shows the result when it ends
    click(&mut h, "button:cloudBackup");
    assert!(h.step_until(T, |h| h.app.session.cloud.job.as_ref().is_some_and(|j| !j.running())), "the backup ends");
    h.step();
    let st = status(&mut h);
    assert!(st["lastBackup"].is_string(), "{st}");
    assert!(acc.join("LightCraft/vault.enc").is_file());

    // Lock, then the passphrase unlocks it again (the buttons are laid out once the job ended)
    h.step();
    click(&mut h, "button:cloudLock");
    assert_eq!(status(&mut h)["unlocked"], false);
    h.step();
    type_into(&mut h, "field:cloudUnlock", PASS);
    click(&mut h, "button:cloudUnlock");
    assert_eq!(status(&mut h)["unlocked"], true);
    // nothing typed here reaches the journal
    assert!(h.app.session.journal.iter().all(|(id, p)| !id.starts_with("cloud.") && !p.to_string().contains(PASS)));

    drop(h);
    for d in [lib, acc] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Google sign-in needs the desktop client's secret as well as its id: Sign In stays off until
/// both are typed, then starts a browser sign-in (a loopback listener; nothing goes out).
#[test]
fn google_sign_in_asks_for_the_client_secret() {
    let lib = temp("gdrive");
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.open_library(&lib, false).unwrap();
    session.execute("cloud.setup", &json!({ "passphrase": PASS })).unwrap();
    let mut h = Headless::new(LightcraftApp::new(session, Services { png: None, ..Default::default() }), [1400.0, 900.0], 1.0);
    h.app.ui.dialog = Some(Dialog::Settings { tab: "cloud".into() });
    h.step();
    h.request("ui.widgets", json!({}), T);
    h.step();
    click(&mut h, "button:cloudSignIn-gdrive");
    type_into(&mut h, "field:cloudClientId", "1234-test.apps.googleusercontent.com");
    click(&mut h, "button:cloudSignInGo");
    assert!(h.app.session.cloud.sign_in.is_none(), "no sign-in without the secret");
    type_into(&mut h, "field:cloudClientSecret", "GOCSPX-test");
    click(&mut h, "button:cloudSignInGo");
    let url = h.app.session.cloud.sign_in.as_ref().map(|s| s.url.clone()).unwrap_or_default();
    assert!(url.starts_with("https://accounts.google.com/") && url.contains("client_id=1234-test"), "{url}");
    assert!(!url.contains("GOCSPX"), "the secret never goes in the browser's address");
    h.app.session.execute("cloud.cancel", &json!({})).unwrap();

    drop(h);
    let _ = std::fs::remove_dir_all(lib);
}

#[path = "../../cloud/tests/common/mod.rs"]
mod fake_http;

/// OpenDrive from Settings: username and password typed, Sign In, the account appears and the
/// password is gone from the form and kept nowhere; when the sign-in stops working the account
/// shows Sign In Again, which fixes the same account.
#[test]
fn opendrive_signs_in_from_settings_and_again_when_it_expires() {
    use fake_http::opendrive::{PASS as OD_PASS, USER, start};
    let (fake, api) = start();
    let lib = temp("opendrive");
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.open_library(&lib, false).unwrap();
    session.execute("cloud.setup", &json!({ "passphrase": PASS })).unwrap();
    session.cloud.opendrive_api = Some(api);
    let mut h = Headless::new(LightcraftApp::new(session, Services { png: None, ..Default::default() }), [1400.0, 900.0], 1.0);
    h.app.ui.dialog = Some(Dialog::Settings { tab: "cloud".into() });
    h.step();
    h.request("ui.widgets", json!({}), T);
    h.step();

    click(&mut h, "button:cloudSignIn-opendrive");
    type_into(&mut h, "field:cloudUsername", USER);
    type_into(&mut h, "field:cloudPassword", OD_PASS);
    click(&mut h, "button:cloudSignInGo");
    let added = |h: &Headless| h.app.session.cloud.vault.as_ref().is_some_and(|v| v.accounts.len() == 1);
    assert!(h.step_until(T, added), "the account is added");
    let account = h.app.session.cloud.vault.as_ref().unwrap().accounts[0].clone();
    assert_eq!((account.provider.as_str(), account.label.as_str()), ("opendrive", USER), "the username tells accounts apart");
    assert!(!account.config.to_string().contains(OD_PASS));
    // the form closed and forgot the password
    h.step();
    assert!(h.request("ui.clickWidget", json!({"id": "field:cloudPassword"}), T)["ok"] == false, "the form is closed");

    // the sign-in ran out: Check Accounts shows it, Sign In Again fixes the same account
    fake.lock().unwrap().refresh = "expired".into();
    h.app.session.execute("cloud.refresh", &json!({"wait": true})).unwrap();
    h.step();
    h.step();
    click(&mut h, "button:cloudSignInAgain-0");
    type_into(&mut h, "field:cloudPassword", OD_PASS);
    click(&mut h, "button:cloudSignInGo");
    let fixed = |h: &Headless| h.app.session.cloud.sign_in.as_ref().is_some_and(|s| s.taken.load(std::sync::atomic::Ordering::Relaxed));
    assert!(h.step_until(T, fixed), "the sign-in finished");
    let st = status(&mut h);
    assert_eq!(st["accounts"].as_array().map(Vec::len), Some(1), "{st}");
    assert_eq!(st["accounts"][0]["id"], account.id.as_str());
    assert!(st["accounts"][0]["error"].is_null(), "{st}");

    drop(h);
    let _ = std::fs::remove_dir_all(lib);
}
