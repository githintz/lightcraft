//! Settings ▸ Cloud: back the library up, encrypted, to the user's own storage accounts
//! (`cloud.*` commands, `docs/cloud-storage.md`). Set up or unlock the vault, connect accounts (a
//! folder, or a browser sign-in for Google Drive, OneDrive and Dropbox), choose copies, back up
//! now with progress. S3, WebDAV and restoring a library are commands for now (CLI, MCP).
//!
//! Nothing here waits on the network: backups and sign-ins run on worker threads and the tab
//! polls their state.

use egui::RichText;
use serde_json::{Value, json};

use super::settings::{check, choices, heading, hint};
use crate::LightcraftApp;
use crate::text_field::TextField;
use crate::theme::Tokens;
use crate::widgets::register;

const LABEL_W: f32 = 150.0;

/// The tab's typing, kept between frames.
#[derive(Clone, Default)]
struct Form {
    pass: String,
    repeat: String,
    /// Which sign-in form is open ("" = none) and its client id.
    signing: String,
    client_id: String,
    /// Google's token endpoint wants the desktop client's secret too (not secret there: RFC 8252 §8.5).
    client_secret: String,
    /// OpenDrive signs in with the account's own username and password (sent once, kept nowhere).
    username: String,
    password: String,
    /// The account the open sign-in form signs in again to ("" = a new account).
    account: String,
}

fn form_id() -> egui::Id {
    egui::Id::new("cloud-settings-form")
}

fn gb(bytes: u64) -> String {
    let g = bytes as f64 / 1_073_741_824.0;
    if g >= 1024.0 {
        let t = g / 1024.0;
        if t.fract() < 0.05 { format!("{t:.0} TB") } else { format!("{t:.1} TB") }
    } else if g >= 100.0 {
        format!("{g:.0} GB")
    } else if g >= 1.0 {
        format!("{g:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1_048_576.0)
    }
}

fn provider_name(kind: &str) -> &str {
    match kind {
        "folder" => "Folder",
        "s3" => "S3-compatible",
        "webdav" => "WebDAV",
        "gdrive" => "Google Drive",
        "onedrive" => "OneDrive",
        "dropbox" => "Dropbox",
        "opendrive" => "OpenDrive",
        other => other,
    }
}

fn row<R>(ui: &mut egui::Ui, t: &Tokens, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(LABEL_W, 24.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_width(LABEL_W);
            ui.label(RichText::new(crate::i18n::tr(label)).color(t.text_label));
        });
        add(ui)
    })
    .inner
}

/// Run a cloud command; an error becomes a toast.
fn run(app: &mut LightcraftApp, ctx: &egui::Context, id: &str, p: Value) -> Option<Value> {
    match app.run(id, p) {
        Ok(v) => Some(v),
        Err(e) => {
            app.toast(ctx, e);
            None
        }
    }
}

pub fn settings_tab(app: &mut LightcraftApp, ui: &mut egui::Ui, t: &Tokens) {
    if cfg!(target_arch = "wasm32") {
        heading(ui, t, "Cloud backup");
        hint(ui, t, "Cloud backup is in the desktop app.");
        return;
    }
    let ctx = ui.ctx().clone();
    let status = app.run("cloud.status", json!({})).unwrap_or(Value::Null);
    heading(ui, t, "Cloud backup");
    hint(
        ui,
        t,
        "Back this library up, encrypted, to your own storage accounts: a disk or NAS folder, Google Drive, OneDrive, Dropbox, OpenDrive. LightCraft runs no server and has no account; only you hold the passphrase.",
    );
    if status["available"] != true {
        hint(ui, t, status["why"].as_str().unwrap_or("Cloud backup needs a library on disk."));
        return;
    }
    let mut form: Form = ui.data(|d| d.get_temp(form_id())).unwrap_or_default();
    if status["configured"] != true {
        setup(app, ui, t, &ctx, &mut form);
    } else if status["unlocked"] != true {
        unlock(app, ui, t, &ctx, &mut form);
    } else {
        accounts(app, ui, t, &ctx, &status, &mut form);
        backup(app, ui, t, &ctx, &status);
    }
    ui.data_mut(|d| d.insert_temp(form_id(), form));
}

fn setup(app: &mut LightcraftApp, ui: &mut egui::Ui, t: &Tokens, ctx: &egui::Context, form: &mut Form) {
    row(ui, t, "Passphrase", |ui| TextField::singleline("field:cloudPassphrase", &mut form.pass).password(true).width(220.0).show(ui));
    row(ui, t, "Repeat it", |ui| TextField::singleline("field:cloudPassphraseRepeat", &mut form.repeat).password(true).width(220.0).show(ui));
    let long = form.pass.chars().count() >= 8;
    let same = form.pass == form.repeat;
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W);
        let r = ui.add_enabled(long && same, egui::Button::new(crate::i18n::tr("Set Up Cloud Backup")));
        register(ui.ctx(), "button:cloudSetup", r.rect);
        if r.clicked() && run(app, ctx, "cloud.setup", json!({"passphrase": form.pass})).is_some() {
            *form = Form::default();
        }
        if !long {
            hint(ui, t, "at least 8 characters");
        } else if !same {
            hint(ui, t, "the two don't match");
        }
    });
    hint(ui, t, "Write the passphrase down: without it the backup can't be opened, by you or anyone else.");
}

fn unlock(app: &mut LightcraftApp, ui: &mut egui::Ui, t: &Tokens, ctx: &egui::Context, form: &mut Form) {
    let r = row(ui, t, "Passphrase", |ui| TextField::singleline("field:cloudUnlock", &mut form.pass).password(true).width(220.0).show(ui));
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W);
        let b = ui.add_enabled(!form.pass.is_empty(), egui::Button::new(crate::i18n::tr("Unlock")));
        register(ui.ctx(), "button:cloudUnlock", b.rect);
        if (b.clicked() || r.committed()) && !form.pass.is_empty() && run(app, ctx, "cloud.unlock", json!({"passphrase": form.pass})).is_some() {
            *form = Form::default();
        }
    });
    hint(ui, t, "Cloud backup is set up for this library and locked.");
}

fn accounts(app: &mut LightcraftApp, ui: &mut egui::Ui, t: &Tokens, ctx: &egui::Context, status: &Value, form: &mut Form) {
    heading(ui, t, "Storage accounts");
    let list = status["accounts"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        hint(ui, t, "No account yet: add a folder (another disk, a NAS) or sign in to a cloud drive.");
    }
    for (i, a) in list.iter().enumerate() {
        let id = a["id"].as_str().unwrap_or_default().to_string();
        let label =
            format!("{} · {}", a["label"].as_str().unwrap_or("?"), crate::i18n::tr(provider_name(a["provider"].as_str().unwrap_or_default())));
        let cap = a["settings"]["cap"].as_u64();
        let detail = match (&a["quota"], a["error"].as_str()) {
            (_, Some(e)) => RichText::new(e.chars().take(120).collect::<String>()).color(t.reject),
            (q, None) if q.is_object() => {
                let used = q["used"].as_u64().unwrap_or(0);
                match (q["total"].as_u64(), cap) {
                    (Some(total), _) => RichText::new(format!("{} of {} used", gb(used), gb(total))).color(t.text_dim),
                    (None, Some(c)) => RichText::new(format!("{} used, limit {}", gb(used), gb(c))).color(t.text_dim),
                    (None, None) => RichText::new(format!("{} used", gb(used))).color(t.text_dim),
                }
            }
            _ => RichText::new(crate::i18n::tr("not checked yet")).color(t.text_dim),
        };
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(LABEL_W + 120.0, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_min_width(LABEL_W + 120.0);
                ui.add(egui::Label::new(RichText::new(&label).color(t.text)).truncate()).on_hover_text(&label);
            });
            ui.label(detail);
            // a sign-in that stopped working (expired, revoked, used up on another computer)
            let kind = a["provider"].as_str().unwrap_or_default();
            let expired = a["error"].as_str().is_some_and(|e| e.contains("sign in again"));
            if expired && ["gdrive", "onedrive", "dropbox", "opendrive"].contains(&kind) {
                let r = ui.small_button(crate::i18n::tr("Sign In Again"));
                register(ui.ctx(), format!("button:cloudSignInAgain-{i}"), r.rect);
                if r.clicked() {
                    form.signing = kind.to_string();
                    form.account = id.clone();
                    form.client_id = status["clientIds"][kind].as_str().unwrap_or_default().to_string();
                }
            }
            let r = ui.small_button(crate::i18n::tr("Remove"));
            register(ui.ctx(), format!("button:cloudRemove-{i}"), r.rect);
            if r.clicked() && run(app, ctx, "cloud.removeAccount", json!({ "id": id })).is_some() {
                app.toast(ctx, crate::i18n::tr("Account removed; files already on it stay there."));
            }
        });
        // placement only uses space it knows about: a folder has no quota until it gets a limit
        let unknown_size = a["quota"]["total"].is_null();
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(RichText::new(crate::i18n::tr("Use up to")).size(11.0).color(t.text_label));
            const G: u64 = 1_073_741_824;
            let mut v = cap.unwrap_or(0);
            let opts =
                [(0u64, if unknown_size { "—" } else { "90 %" }), (50 * G, "50 GB"), (200 * G, "200 GB"), (1024 * G, "1 TB"), (4096 * G, "4 TB")];
            if choices(ui, &format!("cloudCap-{i}"), &opts, &mut v) {
                let cap = if v == 0 { Value::Null } else { json!(v) };
                run(app, ctx, "cloud.setAccount", json!({ "id": id, "cap": cap }));
            }
        });
        if unknown_size && cap.is_none() {
            hint(ui, t, "  This account doesn't report its size: choose a limit so photos can be stored on it.");
        }
    }

    ui.horizontal(|ui| {
        if app.services.pick_folder.is_some() {
            let r = ui.button(crate::i18n::tr("Add Folder…"));
            register(ui.ctx(), "button:cloudAddFolder", r.rect);
            if r.clicked()
                && let Some(dir) = app.services.pick_folder.as_mut().and_then(|f| f())
            {
                let label = std::path::Path::new(&dir).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| dir.clone());
                run(app, ctx, "cloud.addAccount", json!({"provider": "folder", "label": label, "config": {"path": dir}}));
            }
        }
        for (kind, name) in [("gdrive", "Google Drive…"), ("onedrive", "OneDrive…"), ("dropbox", "Dropbox…"), ("opendrive", "OpenDrive…")] {
            let r = ui.add(egui::Button::new(crate::i18n::tr(name)).selected(form.signing == kind));
            register(ui.ctx(), format!("button:cloudSignIn-{kind}"), r.rect);
            if r.clicked() {
                form.signing = if form.signing == kind { String::new() } else { kind.to_string() };
                form.account.clear();
                form.client_id = status["clientIds"][kind].as_str().unwrap_or_default().to_string();
            }
        }
    });
    if form.signing == "opendrive" {
        row(ui, t, "Username or email", |ui| TextField::singleline("field:cloudUsername", &mut form.username).width(300.0).show(ui));
        let entered =
            row(ui, t, "Password", |ui| TextField::singleline("field:cloudPassword", &mut form.password).password(true).width(300.0).show(ui));
        ui.horizontal(|ui| {
            ui.add_space(LABEL_W);
            let ready = !form.username.trim().is_empty() && !form.password.is_empty();
            let r = ui.add_enabled(ready, egui::Button::new(crate::i18n::tr("Sign In")));
            register(ui.ctx(), "button:cloudSignInGo", r.rect);
            if ready && (r.clicked() || entered.committed()) {
                run(
                    app,
                    ctx,
                    "cloud.signIn",
                    json!({"provider": "opendrive", "username": form.username.trim(), "password": form.password, "account": Some(&form.account).filter(|a| !a.is_empty())}),
                );
                // the password goes no further than the sign-in
                form.password.clear();
                form.signing.clear();
                form.account.clear();
            }
        });
        hint(
            ui,
            t,
            "LightCraft trades the password for a sign-in token and keeps only the token. It lasts 30 days from the last backup or check: back up at least once a month.",
        );
    } else if !form.signing.is_empty() {
        let kind = form.signing.clone();
        row(ui, t, "Client ID", |ui| TextField::singleline("field:cloudClientId", &mut form.client_id).width(300.0).show(ui));
        if kind == "gdrive" {
            row(ui, t, "Client secret", |ui| {
                TextField::singleline("field:cloudClientSecret", &mut form.client_secret).password(true).width(300.0).show(ui)
            });
        }
        ui.horizontal(|ui| {
            ui.add_space(LABEL_W);
            let ready = !form.client_id.trim().is_empty() && (kind != "gdrive" || !form.client_secret.trim().is_empty());
            let r = ui.add_enabled(ready, egui::Button::new(crate::i18n::tr("Sign In in the Browser")));
            register(ui.ctx(), "button:cloudSignInGo", r.rect);
            if r.clicked()
                && let Some(v) = run(app, ctx, "cloud.signIn", json!({"provider": kind, "clientId": form.client_id.trim(), "clientSecret": Some(form.client_secret.trim()).filter(|s| !s.is_empty()), "account": Some(&form.account).filter(|a| !a.is_empty())}))
            {
                if let Some(url) = v["url"].as_str()
                    && let Err(e) = crate::links::open(app, url)
                {
                    app.toast(ctx, format!("Open this address in a browser: {url} ({e})"));
                }
                form.signing.clear();
                form.client_secret.clear();
                form.account.clear();
            }
        });
        hint(ui, t, "The client id of your app registration with this provider (docs/cloud-storage.md says how to make one).");
    }
    sign_in_progress(app, ui, t, ctx, status);
}

fn sign_in_progress(app: &mut LightcraftApp, ui: &mut egui::Ui, t: &Tokens, ctx: &egui::Context, status: &Value) {
    if status["signIn"].is_null() {
        return;
    }
    let s = app.run("cloud.signInStatus", json!({})).unwrap_or(Value::Null);
    match s["state"].as_str() {
        Some("waiting") => {
            ui.horizontal(|ui| {
                ui.spinner();
                let text = if s["provider"] == "opendrive" { "Signing in to OpenDrive…" } else { "Waiting for the sign-in in your browser…" };
                ui.label(RichText::new(crate::i18n::tr(text)).color(t.text));
                let r = ui.small_button(crate::i18n::tr("Cancel"));
                register(ui.ctx(), "button:cloudSignInCancel", r.rect);
                if r.clicked() {
                    let _ = app.run("cloud.cancel", json!({}));
                }
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }
        Some("failed") => hint(ui, t, &format!("Sign-in failed: {}", s["error"].as_str().unwrap_or("?"))),
        _ => {}
    }
}

fn backup(app: &mut LightcraftApp, ui: &mut egui::Ui, t: &Tokens, ctx: &egui::Context, status: &Value) {
    heading(ui, t, "Backup");
    row(ui, t, "Copies of originals", |ui| {
        let mut v = status["originalCopies"].as_u64().unwrap_or(1);
        if choices(ui, "cloudCopies", &[(1u64, "1"), (2, "2"), (3, "3")], &mut v) {
            run(app, ctx, "cloud.configure", json!({"originalCopies": v}));
        }
    });
    hint(ui, t, "Copies go to accounts at different providers, so one provider closing an account loses nothing. The catalog goes to every account.");
    let mut smart = status["smartPreviews"].as_bool().unwrap_or(true);
    if check(ui, "cloud.smartPreviews", &mut smart, "Include smart previews (edit photos whose original is offline)") {
        run(app, ctx, "cloud.configure", json!({"smartPreviews": smart}));
    }
    let job = &status["job"];
    let running = job["running"] == true;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let can = !running && !status["accounts"].as_array().is_none_or(Vec::is_empty);
        let r = ui.add_enabled(can, egui::Button::new(crate::i18n::tr("Back Up Now")));
        register(ui.ctx(), "button:cloudBackup", r.rect);
        if r.clicked() {
            run(app, ctx, "cloud.backup", json!({}));
        }
        if running {
            ui.spinner();
            let (done, total) = (job["done"].as_u64().unwrap_or(0), job["total"].as_u64().unwrap_or(0));
            let what = if job["what"] == "refresh" { crate::i18n::tr("Checking accounts…").to_string() } else { format!("{done} of {total}") };
            ui.label(RichText::new(what).color(t.text));
            let c = ui.small_button(crate::i18n::tr("Stop"));
            register(ui.ctx(), "button:cloudCancel", c.rect);
            if c.clicked() {
                let _ = app.run("cloud.cancel", json!({}));
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        } else {
            let r = ui.add_enabled(!running, egui::Button::new(crate::i18n::tr("Check Accounts")));
            register(ui.ctx(), "button:cloudRefresh", r.rect);
            if r.clicked() {
                run(app, ctx, "cloud.refresh", json!({}));
            }
            let l = ui.button(crate::i18n::tr("Lock"));
            register(ui.ctx(), "button:cloudLock", l.rect);
            if l.clicked() {
                let _ = app.run("cloud.lock", json!({}));
            }
        }
    });
    if !running && job.is_object() {
        let text = if let Some(e) = job["error"].as_str() {
            RichText::new(format!("Last {} failed: {e}", job["what"].as_str().unwrap_or("job"))).color(t.reject)
        } else if job["what"] == "backup" {
            let failed = job["failed"].as_u64().unwrap_or(0);
            let line = format!(
                "{} sent, {} already stored{}",
                job["stored"].as_u64().unwrap_or(0),
                job["unchanged"].as_u64().unwrap_or(0),
                if failed > 0 { format!(", {failed} failed") } else { String::new() }
            );
            RichText::new(line).color(if failed > 0 { t.reject } else { t.text_dim })
        } else {
            RichText::new("")
        };
        ui.label(text.size(11.0));
        if let Some(first) = job["failures"].as_array().and_then(|f| f.first()) {
            hint(ui, t, &format!("{}: {}", first["name"].as_str().unwrap_or("?"), first["error"].as_str().unwrap_or("?")));
        }
    }
    if let Some(last) = status["lastBackup"].as_str() {
        hint(ui, t, &format!("Last backup: {last}"));
    }
    hint(
        ui,
        t,
        "S3-compatible and WebDAV accounts, and restoring a library on another computer, are lightcraft-cli commands for now (cloud.addAccount, cloud.restore).",
    );
}
