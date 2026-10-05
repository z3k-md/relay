//! Windows glue between the explorer window and `relay-shell-win`: STA pools
//! for images and file operations, the context menu and drag and drop on
//! Tauri's main thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use relay_explorer::effect::{self, Effect, Keys};
use relay_shell_win::dnd::{self, DragState, DropData, DropHandler, Registration};
use relay_shell_win::fileop::{self, Transfer};
use relay_shell_win::image::{self, Kind};
use relay_shell_win::menu;
use relay_shell_win::sta::StaPool;
use tauri::WebviewWindow;
use tauri::ipc::Channel;
use tokio::sync::oneshot;

use super::DragEvent;

type IconCache = Mutex<HashMap<(String, u32), Arc<Vec<u8>>>>;

/// Shell worker threads, shared by every explorer window.
#[derive(Clone)]
pub struct Shell {
    images: StaPool,
    ops: StaPool,
    icons: Arc<IconCache>,
}

impl Shell {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            images: StaPool::new("relay-shell-img", 4)?,
            ops: StaPool::new("relay-shell-op", 2)?,
            icons: Arc::default(),
        })
    }

    /// Render an icon (or a thumbnail) and hand the PNG to `done`, from a
    /// pool thread or, on a cache hit, right away. Newest requests run first:
    /// they are for what is on screen now.
    pub fn image(
        &self,
        path: PathBuf,
        size: u32,
        thumbnail: bool,
        cache_key: Option<String>,
        done: impl FnOnce(Option<Arc<Vec<u8>>>) + Send + 'static,
    ) {
        let size = image::snap_size(size);
        let key = cache_key.map(|k| (k, size));
        if let Some(key) = &key {
            let hit = self
                .icons
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(key)
                .cloned();
            if hit.is_some() {
                return done(hit);
            }
        }
        let icons = Arc::clone(&self.icons);
        self.images.spawn_first(move || {
            let kind = if thumbnail {
                Kind::Thumbnail
            } else {
                Kind::Icon
            };
            let png = match image::render_png(&path, size, kind) {
                Ok(png) => Some(Arc::new(png)),
                Err(err) => {
                    log::debug!("explorer image {}: {err}", path.display());
                    None
                }
            };
            if let (Some(key), Some(png)) = (key, &png) {
                icons
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, Arc::clone(png));
            }
            done(png);
        });
    }
}

fn owner(window: &WebviewWindow) -> Result<isize, String> {
    Ok(window.hwnd().map_err(|e| e.to_string())?.0 as isize)
}

/// Run `f` on Tauri's main thread (the window's thread, an OLE STA) and
/// wait for its result.
async fn on_main<T: Send + 'static>(
    window: &WebviewWindow,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = oneshot::channel();
    window
        .run_on_main_thread(move || {
            let _ = tx.send(f());
        })
        .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())?
}

pub async fn context_menu(
    window: WebviewWindow,
    folder: String,
    names: Vec<String>,
    x: f64,
    y: f64,
    extended: bool,
) -> Result<Option<String>, String> {
    let owner = owner(&window)?;
    let origin = window.inner_position().map_err(|e| e.to_string())?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let screen_x = origin.x + (x * scale).round() as i32;
    let screen_y = origin.y + (y * scale).round() as i32;
    on_main(&window, move || {
        let folder = PathBuf::from(folder);
        let paths: Vec<PathBuf> = names.iter().map(|n| folder.join(n)).collect();
        let target = if paths.is_empty() {
            menu::Target::Background(&folder)
        } else {
            menu::Target::Items(&paths)
        };
        let outcome =
            menu::show(owner, target, screen_x, screen_y, extended).map_err(|e| e.to_string())?;
        Ok((!outcome.invoked).then_some(outcome.verb).flatten())
    })
    .await
}

pub async fn start_drag(
    window: WebviewWindow,
    folder: String,
    names: Vec<String>,
) -> Result<u32, String> {
    let owner = owner(&window)?;
    on_main(&window, move || {
        let folder = PathBuf::from(folder);
        let paths: Vec<PathBuf> = names.iter().map(|n| folder.join(n)).collect();
        dnd::start_drag(
            owner,
            &paths,
            dnd::EFFECT_COPY | dnd::EFFECT_MOVE | dnd::EFFECT_LINK,
        )
        .map_err(|e| e.to_string())
    })
    .await
}

thread_local! {
    /// Drop target registrations by window label. Main thread only.
    static DROPS: RefCell<HashMap<String, Registration>> = RefCell::new(HashMap::new());
}

pub async fn register_drop_target(
    window: WebviewWindow,
    shell: Shell,
    drop_target: Arc<Mutex<Option<PathBuf>>>,
    events: Channel<DragEvent>,
) -> Result<bool, String> {
    let owner = owner(&window)?;
    let label = window.label().to_string();
    on_main(&window, move || {
        // Revoke the old registration (a page reload) before registering
        // again, or dropping it later would revoke the new one.
        DROPS.with(|d| d.borrow_mut().remove(&label));
        let handler = Rc::new(Drops {
            shell,
            owner,
            drop_target,
            events,
            sources: RefCell::new(Vec::new()),
            virtual_count: Cell::new(0),
        });
        let registration = dnd::register(owner, handler).map_err(|e| e.to_string())?;
        DROPS.with(|d| d.borrow_mut().insert(label, registration));
        Ok(true)
    })
    .await
}

/// Forget a closed window's registration. Call on the main thread.
pub fn forget_drop_target(label: &str) {
    DROPS.with(|d| d.borrow_mut().remove(label));
}

struct Drops {
    shell: Shell,
    owner: isize,
    /// The folder under the pointer, as the frontend last reported it.
    drop_target: Arc<Mutex<Option<PathBuf>>>,
    events: Channel<DragEvent>,
    /// Paths on offer, read once on enter.
    sources: RefCell<Vec<PathBuf>>,
    virtual_count: Cell<usize>,
}

impl Drops {
    fn target(&self) -> Option<PathBuf> {
        self.drop_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn effect(&self, state: &DragState, target: &Path) -> u32 {
        let sources = self.sources.borrow();
        if sources.is_empty() {
            return if self.virtual_count.get() > 0 {
                dnd::EFFECT_COPY
            } else {
                dnd::EFFECT_NONE
            };
        }
        // A folder can't go inside itself.
        if sources.iter().any(|s| target.starts_with(s)) {
            return dnd::EFFECT_NONE;
        }
        let keys = Keys {
            ctrl: state.ctrl,
            shift: state.shift,
            alt: state.alt,
        };
        let already_there = sources.iter().all(|s| s.parent() == Some(target));
        match effect::choose(
            keys,
            state.allowed & dnd::EFFECT_COPY != 0,
            state.allowed & dnd::EFFECT_MOVE != 0,
            effect::same_volume(&sources[0], target),
            already_there,
        ) {
            Effect::None => dnd::EFFECT_NONE,
            Effect::Copy => dnd::EFFECT_COPY,
            Effect::Move => dnd::EFFECT_MOVE,
        }
    }

    fn done(&self, target: &Path, started: Instant, result: Result<bool, String>) {
        send_done(&self.events, target, started, result);
    }
}

fn send_done(
    events: &Channel<DragEvent>,
    target: &Path,
    started: Instant,
    result: Result<bool, String>,
) {
    let (ok, message) = match result {
        Ok(true) => (true, None),
        Ok(false) => (false, Some("cancelled".to_string())),
        Err(err) => (false, Some(err)),
    };
    let _ = events.send(DragEvent::Done {
        target: target.to_string_lossy().into_owned(),
        ok,
        message,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    });
}

impl DropHandler for Drops {
    fn over(&self, state: &DragState, data: &DropData<'_>, enter: bool) -> u32 {
        let mut formats = Vec::new();
        if enter {
            let paths = data.paths();
            let virtual_count = if paths.is_empty() {
                data.virtual_files().len()
            } else {
                0
            };
            *self.sources.borrow_mut() = paths;
            self.virtual_count.set(virtual_count);
            formats = data.formats();
        }
        let effect = self
            .target()
            .map_or(dnd::EFFECT_NONE, |t| self.effect(state, &t));
        let _ = self.events.send(DragEvent::Over {
            x: state.client_x,
            y: state.client_y,
            enter,
            count: self.sources.borrow().len(),
            virtual_count: self.virtual_count.get(),
            formats,
            effect,
        });
        effect
    }

    fn leave(&self) {
        self.sources.borrow_mut().clear();
        self.virtual_count.set(0);
        let _ = self.events.send(DragEvent::Leave);
    }

    fn dropped(&self, state: &DragState, data: &DropData<'_>) -> u32 {
        let Some(target) = self.target() else {
            self.leave();
            return dnd::EFFECT_NONE;
        };
        let effect = self.effect(state, &target);
        let sources = std::mem::take(&mut *self.sources.borrow_mut());
        let count = if sources.is_empty() {
            self.virtual_count.get()
        } else {
            sources.len()
        };
        let _ = self.events.send(DragEvent::Dropped {
            effect,
            target: target.to_string_lossy().into_owned(),
            count,
        });
        if effect == dnd::EFFECT_NONE {
            return dnd::EFFECT_NONE;
        }
        let started = Instant::now();

        if sources.is_empty() {
            // Virtual files must be read before the drop returns.
            let result = data
                .save_virtual_files(&target)
                .map(|_| true)
                .map_err(|e| e.to_string());
            self.done(&target, started, result);
            return dnd::EFFECT_COPY;
        }

        let kind = if effect == dnd::EFFECT_MOVE {
            Transfer::Move
        } else {
            Transfer::Copy
        };
        let owner = self.owner;
        let events = self.events.clone();
        self.shell.ops.spawn(move || {
            let result =
                fileop::transfer(owner, &sources, &target, kind).map_err(|e| e.to_string());
            send_done(&events, &target, started, result);
        });
        // We perform moves ourselves, so tell the source "copy": a source
        // told "move" deletes its originals, racing our copy engine.
        dnd::EFFECT_COPY
    }
}
