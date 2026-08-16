//! Device-flow login dialog.
//!
//! Threading: `reqwest` futures need a tokio reactor, and GTK widgets may only
//! be touched from the glib main context. The bridge is one `async_channel`
//! per login — the poll loop runs on the tokio runtime and sends its outcome;
//! a `spawn_future_local` on the main context receives it and updates widgets.
//! Nothing GTK-shaped ever crosses to the worker.

use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

use auth::{device_flow, gh_import, store, Account, AuthError, Token, BASE_SCOPES, CLIENT_ID};

/// Outcome handed back to the caller once the dialog closes.
pub type OnLogin = Rc<dyn Fn(Account, Token)>;

/// Present the login dialog.
///
/// Offers the `gh` import first when one is available: most people who want
/// this app already have `gh` authenticated, and adopting that token turns
/// first run into zero steps.
pub fn present(parent: &impl IsA<gtk::Window>, rt: tokio::runtime::Handle, on_login: OnLogin) {
    let host = auth::DEFAULT_HOST.to_owned();

    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .title("Sign in to GitHub")
        .build();

    let page = adw::ToolbarView::new();
    page.add_top_bar(&adw::HeaderBar::new());

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);

    let status = gtk::Label::new(Some("Choose how to sign in."));
    status.set_wrap(true);
    status.set_xalign(0.0);
    content.append(&status);

    // --- reuse an existing gh login -----------------------------------------
    if let Ok(Some(token)) = gh_import::import(&host) {
        let btn = gtk::Button::with_label("Use my existing GitHub CLI login");
        btn.add_css_class("suggested-action");
        let dialog_ = dialog.clone();
        let on_login_ = on_login.clone();
        let host_ = host.clone();
        let rt_ = rt.clone();
        let status_ = status.clone();

        btn.connect_clicked(move |b| {
            b.set_sensitive(false);
            status_.set_text("Verifying token…");

            let (tx, rx) = async_channel::bounded(1);
            let token = token.clone();
            let host = host_.clone();
            rt_.spawn(async move {
                let _ = tx.send(verify(&host, token).await).await;
            });

            let dialog = dialog_.clone();
            let on_login = on_login_.clone();
            let status = status_.clone();
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok((account, token))) => {
                        // Persist before signalling success: a caller that
                        // starts fetching should never race the token landing.
                        if let Err(e) = store::save(&account, &token) {
                            status.set_text(&format!("Could not save credentials: {e}"));
                            return;
                        }
                        on_login(account, token);
                        dialog.close();
                    }
                    Ok(Err(e)) => status.set_text(&format!("That token did not work: {e}")),
                    Err(_) => status.set_text("Login cancelled."),
                }
            });
        });
        content.append(&btn);

        let sep = gtk::Label::new(Some("or"));
        sep.add_css_class("dim-label");
        content.append(&sep);
    }

    // --- device flow --------------------------------------------------------
    let code_label = gtk::Label::new(None);
    code_label.add_css_class("title-1");
    code_label.add_css_class("monospace");
    code_label.set_selectable(true);
    code_label.set_visible(false);
    content.append(&code_label);

    let open_btn = gtk::Button::with_label("Sign in with a browser");
    open_btn.add_css_class("pill");
    content.append(&open_btn);

    // Without a real App id the device flow fails with
    // `incorrect_client_credentials`, which reads like a GitHub outage rather
    // than a build-configuration problem. Say so up front instead.
    if !auth::client_id_is_configured() {
        open_btn.set_sensitive(false);
        open_btn.set_tooltip_text(Some(
            "This build has no GitHub App client id. Rebuild with \
             FORQEN_CLIENT_ID=<id> to enable browser sign-in.",
        ));
        status.set_text(
            "Browser sign-in is unavailable: this build was compiled without a \
             GitHub App client id.",
        );
    }

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    content.append(&spinner);

    {
        let status = status.clone();
        let code_label = code_label.clone();
        let spinner = spinner.clone();
        let dialog_ = dialog.clone();
        let on_login = on_login.clone();
        let host = host.clone();

        open_btn.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            status.set_text("Contacting GitHub…");

            let (tx, rx) = async_channel::bounded(1);
            let host_ = host.clone();
            let rt_ = rt.clone();

            // Stage one: fetch the code pair. Kept separate from polling so the
            // code can be shown the instant it exists rather than after login.
            let tx_code = tx.clone();
            rt.spawn(async move {
                let http = reqwest_client();
                let base = format!("https://{host_}");
                match device_flow::start(&http, &base, CLIENT_ID, BASE_SCOPES).await {
                    Err(e) => {
                        let _ = tx_code.send(Stage::Failed(e.to_string())).await;
                    }
                    Ok(code) => {
                        let _ = tx_code
                            .send(Stage::Code {
                                user_code: code.user_code.clone(),
                                uri: code.verification_uri.clone(),
                            })
                            .await;

                        let result =
                            device_flow::poll_until_complete(&http, &base, CLIENT_ID, &code).await;
                        let msg = match result {
                            Ok(token) => match verify(&host_, token).await {
                                Ok((a, t)) => Stage::Done(Box::new((a, t))),
                                Err(e) => Stage::Failed(e.to_string()),
                            },
                            Err(e) => Stage::Failed(e.to_string()),
                        };
                        let _ = tx_code.send(msg).await;
                    }
                }
                let _ = rt_;
            });

            let status = status.clone();
            let code_label = code_label.clone();
            let spinner = spinner.clone();
            let dialog = dialog_.clone();
            let on_login = on_login.clone();

            glib::spawn_future_local(async move {
                while let Ok(stage) = rx.recv().await {
                    match stage {
                        Stage::Code { user_code, uri } => {
                            code_label.set_text(&user_code);
                            code_label.set_visible(true);
                            spinner.set_visible(true);
                            spinner.start();
                            status.set_text(&format!(
                                "Enter this code at {uri}\nWaiting for approval…"
                            ));

                            // Copying beats retyping an 8-character code.
                            if let Some(clip) = gtk::gdk::Display::default().map(|d| d.clipboard())
                            {
                                clip.set_text(&user_code);
                            }
                            if let Ok(uri) = glib::Uri::parse(&uri, glib::UriFlags::NONE) {
                                let launcher = gtk::UriLauncher::new(&uri.to_str());
                                launcher.launch(
                                    None::<&gtk::Window>,
                                    None::<&gtk::gio::Cancellable>,
                                    |_| {},
                                );
                            }
                        }
                        Stage::Done(payload) => {
                            let (account, token) = *payload;
                            spinner.stop();
                            if let Err(e) = store::save(&account, &token) {
                                status.set_text(&format!("Could not save credentials: {e}"));
                                return;
                            }
                            on_login(account, token);
                            dialog.close();
                            return;
                        }
                        Stage::Failed(msg) => {
                            spinner.stop();
                            spinner.set_visible(false);
                            code_label.set_visible(false);
                            status.set_text(&msg);
                            return;
                        }
                    }
                }
            });
        });
    }

    page.set_content(Some(&content));
    dialog.set_content(Some(&page));
    dialog.present();
}

enum Stage {
    Code { user_code: String, uri: String },
    Done(Box<(Account, Token)>),
    Failed(String),
}

fn reqwest_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("forqen/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("static client config is valid")
}

/// Confirm a token works and learn which login it belongs to.
///
/// The account name is not in the token response, and it is the keyring key —
/// so this call is not optional even when the token is known-good.
async fn verify(host: &str, token: Token) -> Result<(Account, Token), AuthError> {
    // A throwaway in-memory cache: this runs before the real store exists, and
    // caching a one-shot identity check would serve no one.
    let cache = Arc::new(db::Db::open_in_memory().map_err(|e| AuthError::Malformed {
        host: host.to_owned(),
        detail: e.to_string(),
    })?);

    let probe = Account {
        host: host.to_owned(),
        login: String::new(),
        is_default_for_host: true,
    };

    let client = github::Client::new(probe, token.access.clone(), cache).map_err(|e| {
        AuthError::Malformed {
            host: host.to_owned(),
            detail: e.to_string(),
        }
    })?;

    let user = client.current_user().await.map_err(|e| match e {
        github::GhError::Unauthorized => AuthError::Oauth("token rejected".into()),
        other => AuthError::Malformed {
            host: host.to_owned(),
            detail: other.to_string(),
        },
    })?;

    Ok((
        Account {
            host: host.to_owned(),
            login: user.data.login,
            is_default_for_host: true,
        },
        token,
    ))
}
