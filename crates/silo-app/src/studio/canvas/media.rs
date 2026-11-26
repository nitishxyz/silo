//! Media on the board: videos (assets and video cards' takes) play muted while hovered, at most [`MAX_PLAYING`]
//! at once (the oldest stops; one the pointer left or scrolled out of view stops), through the store's cached file; files from Finder
//! or a Files block dropped on the board, or picked with Import, are `studio.import`ed and placed
//! ([`super::board::Board::place_imports`]); Save writes a file's bytes where the user picks.

use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Instant};

use gpui::{Context, Window};
use silo_proto::studio::{NodeBody, StudioImportParams, StudioImported, StudioMedia};

use super::CanvasView;
use crate::{
    agent::av,
    studio::{
        board::{self, Board, Hit, Imported},
        store::{Asset, AssetKind, Store},
    },
};

const MAX_PLAYING: usize = 4;

struct Playing {
    path: String,
    video: av::Video,
    since: Instant,
}

#[derive(Default)]
pub(crate) struct Hover {
    playing: Vec<Playing>,
    /// The pointer is over the board (a leave is sent once).
    pub over: bool,
}

/// The video file a node shows, root-relative.
fn video_of(body: &NodeBody) -> Option<&str> {
    match body {
        NodeBody::Asset { path, media: StudioMedia::Video, .. } => Some(path),
        b @ NodeBody::Gen { .. } => b.take().filter(|t| t.media == StudioMedia::Video).map(|t| t.path.as_str()),
        _ => None,
    }
}

impl Hover {
    /// Start the hovered video, stop the others (the pointer left them, or they are out of view), loop the ended
    /// ones; frames keep coming while any play.
    pub(super) fn tick(&mut self, board: &Board, store: &gpui::Entity<Store>, window: &mut Window, cx: &mut Context<CanvasView>) {
        let canvas = board.canvas();
        let hovered = match board.hovered() {
            Hit::Node { id, .. } => canvas.node(id).and_then(|n| video_of(&n.body)),
            _ => None,
        };
        if let Some(path) = hovered
            && !self.playing.iter().any(|p| p.path == path)
            && let Asset::Clip(clip) = store.update(cx, |s, cx| s.asset(path, AssetKind::Video, cx))
            && let Some(video) = av::Video::new(&clip.file)
        {
            video.player.set_muted(true);
            video.player.play();
            self.playing.push(Playing { path: path.to_owned(), video, since: Instant::now() });
        }
        if self.playing.is_empty() {
            return;
        }
        let shown: Vec<&str> = board.visible().filter_map(|l| video_of(&canvas.nodes[l.index].body)).collect();
        keep_playing(&mut self.playing, |p| (p.path.as_str(), p.since), hovered, &shown);
        for p in &self.playing {
            if p.video.player.ended() {
                p.video.player.seek(0.0);
                p.video.player.play();
            }
        }
        if !self.playing.is_empty() {
            window.request_animation_frame();
        }
    }

    /// The frame to draw over `path`'s poster, while it plays.
    pub(super) fn frame(&self, path: &str) -> Option<av::Frame> {
        self.playing.iter().find(|p| p.path == path)?.video.frame()
    }
}

impl CanvasView {
    /// Import: pick files, then import them at canvas point `at`.
    pub(super) fn import_dialog(&mut self, at: board::Point, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Import".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update(cx, |this, cx| this.import_paths(paths, Some(at), cx));
        })
        .detach();
    }

    /// The canvas point under the pointer.
    fn pointer_at(&self, window: &Window) -> board::Point {
        let p = window.mouse_position() - self.bounds.get().origin;
        self.board.camera().to_canvas(board::Point::new(f64::from(f32::from(p.x)), f64::from(f32::from(p.y))))
    }

    pub(super) fn drop_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.pointer_at(window);
        self.import_paths(paths, Some(at), cx);
    }

    pub(super) fn drop_files(&mut self, d: &crate::files::DraggedFiles, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = d.local_root.clone() else {
            return self.say("Drop files from a Files block on this Mac, or from Finder", cx);
        };
        let at = self.pointer_at(window);
        let paths = d.paths.iter().map(|p| root.join(p)).collect();
        self.import_paths(paths, Some(at), cx);
    }

    /// `studio.import` each file, then place them all at `at`.
    pub(super) fn import_paths(&mut self, paths: Vec<PathBuf>, at: Option<board::Point>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let left = Rc::new(RefCell::new((paths.len(), Vec::<Imported>::new())));
        let block_id = self.store.read(cx).block_id.clone();
        for path in paths {
            let local = path.clone();
            let method = silo_proto::control::Method::StudioImport(StudioImportParams {
                block_id: block_id.clone(),
                canvas: self.name.clone(),
                from: path.to_string_lossy().into_owned(),
            });
            let caller = self.store.read(cx).caller();
            let left = left.clone();
            caller.call(method, cx, move |this: &mut Self, r: anyhow::Result<StudioImported>, cx| {
                match r {
                    Ok(done) => {
                        let media = board::media_of_path(&done.path);
                        let dims = (media == StudioMedia::Image).then(|| image::image_dimensions(&local).ok()).flatten();
                        left.borrow_mut().1.push(Imported {
                            path: done.path,
                            media,
                            width: dims.map(|d| d.0),
                            height: dims.map(|d| d.1),
                            seconds: None,
                        });
                    }
                    Err(e) => this.say(
                        format!(
                            "Couldn't import {}: {}",
                            local.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                            crate::studio::store::describe(&e)
                        ),
                        cx,
                    ),
                }
                let mut l = left.borrow_mut();
                l.0 -= 1;
                if l.0 == 0 && !l.1.is_empty() {
                    let files = std::mem::take(&mut l.1);
                    drop(l);
                    this.later(cx, move |this, window, cx| {
                        let fx = this.board.place_imports(&files, at);
                        this.apply(fx, window, cx);
                    });
                }
            });
        }
    }

    /// Save: the file's bytes written where the user picks.
    pub(super) fn save_file(&mut self, path: String, cx: &mut Context<Self>) {
        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
        let dir = dirs_downloads();
        let dest = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dest))) = dest.await else { return };
            let _ = this.update(cx, |this, cx| {
                let store = this.store.clone();
                store.read(cx).caller().read_file(&path, cx, move |this: &mut Self, r, cx| {
                    match r.and_then(|bytes| std::fs::write(&dest, bytes).map_err(anyhow::Error::from)) {
                        Ok(()) => this.say(format!("Saved {}", dest.display()), cx),
                        Err(e) => this.say(format!("Couldn't save it: {e:#}"), cx),
                    }
                });
            });
        })
        .detach();
    }

    /// Run `f` with the window once this update is done (effects that focus or open things need it).
    pub(super) fn later(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static) {
        let Some(handle) = self.window else { return };
        let entity = cx.entity();
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| entity.update(cx, |this, cx| f(this, window, cx)));
        });
    }
}

/// The videos still playing: the hovered one if it is in view (dropping a video stops it), at most
/// [`MAX_PLAYING`], the newest kept.
pub(super) fn keep_playing<T>(playing: &mut Vec<T>, of: impl Fn(&T) -> (&str, Instant), hovered: Option<&str>, shown: &[&str]) {
    playing.retain(|p| {
        let path = of(p).0;
        hovered == Some(path) && shown.contains(&path)
    });
    if playing.len() > MAX_PLAYING {
        playing.sort_by_key(|p| std::cmp::Reverse(of(p).1));
        playing.truncate(MAX_PLAYING);
    }
}

fn dirs_downloads() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), |h| PathBuf::from(h).join("Downloads"))
}
