//! Developer tool: render the window offscreen to PNG.
//!
//!   cargo run --release --features snapshot -- --snapshot OUT.png [FILE] [size=WxH]
//!       [search=TEXT] [sort=COL] [desc] [select=R,C:R,C] [menu=COL] [ctx=R,C]
//!       [filter=COL:TEXT] [scroll=ROW] [edit=TEXT] [zoom=F] [frames=N]
//!
//! With `frames=N` it also prints main-thread CPU per frame while scrolling.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext as _, HeadlessAppContext, point, px, size};

use crate::app::Workspace;
use crate::data::query::{Cond, TextOp};

#[derive(Default)]
struct Opts {
    search: Option<String>,
    sort: Option<(usize, bool)>,
    select: Option<((u64, usize), (u64, usize))>,
    menu: Option<usize>,
    ctx: Option<(u64, usize)>,
    filter: Option<(usize, String)>,
    scroll: Option<u64>,
    edit: Option<String>,
    zoom: Option<f32>,
    frames: usize,
}

fn cell(s: &str) -> (u64, usize) {
    let (r, c) = s.split_once(',').unwrap();
    (r.parse().unwrap(), c.parse().unwrap())
}

fn thread_cpu() -> f64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

pub fn run(args: &[String]) {
    let dir = std::env::temp_dir().join(format!("rowscope-snap-{}", std::process::id()));
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.join("config")) };
    let out = PathBuf::from(&args[0]);
    let mut paths = Vec::new();
    let mut dims = (1360.0, 860.0);
    let mut o = Opts::default();
    let mut desc = false;
    for a in &args[1..] {
        if let Some(v) = a.strip_prefix("size=") {
            let (w, h) = v.split_once('x').unwrap();
            dims = (w.parse().unwrap(), h.parse().unwrap());
        } else if let Some(v) = a.strip_prefix("search=") {
            o.search = Some(v.into());
        } else if let Some(v) = a.strip_prefix("sort=") {
            o.sort = Some((v.parse().unwrap(), false));
        } else if a == "desc" {
            desc = true;
        } else if let Some(v) = a.strip_prefix("select=") {
            let (a, b) = v.split_once(':').unwrap_or((v, v));
            o.select = Some((cell(a), cell(b)));
        } else if let Some(v) = a.strip_prefix("menu=") {
            o.menu = Some(v.parse().unwrap());
        } else if let Some(v) = a.strip_prefix("ctx=") {
            o.ctx = Some(cell(v));
        } else if let Some(v) = a.strip_prefix("filter=") {
            let (c, t) = v.split_once(':').unwrap();
            o.filter = Some((c.parse().unwrap(), t.into()));
        } else if let Some(v) = a.strip_prefix("scroll=") {
            o.scroll = Some(v.parse().unwrap());
        } else if let Some(v) = a.strip_prefix("edit=") {
            o.edit = Some(v.into());
        } else if let Some(v) = a.strip_prefix("zoom=") {
            o.zoom = Some(v.parse().unwrap());
        } else if let Some(v) = a.strip_prefix("frames=") {
            o.frames = v.parse().unwrap();
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    if let Some(s) = &mut o.sort {
        s.1 = desc;
    }

    let text = Arc::new(gpui_wgpu::CosmicTextSystem::new("Noto Sans"));
    let mut cx = HeadlessAppContext::with_platform(text, Arc::new(crate::assets::Assets), || {
        gpui_platform::current_headless_renderer()
    });
    cx.update(crate::bind_keys);
    let window = cx
        .open_window(size(px(dims.0), px(dims.1)), |window, cx| {
            cx.new(|cx| Workspace::new(paths, window, cx))
        })
        .unwrap();
    let any: gpui::AnyWindowHandle = window.into();
    let busy = |cx: &mut HeadlessAppContext| {
        cx.update_window(any, |root, _, cx| {
            let ws = root.downcast::<Workspace>().unwrap();
            ws.read(cx).sheet().is_some_and(|s| {
                let s = s.read(cx);
                s.loading.is_some() || s.query_running
            })
        })
        .unwrap()
    };
    let settle = |cx: &mut HeadlessAppContext| {
        let t0 = std::time::Instant::now();
        let mut quiet = 0;
        while quiet < 10 && t0.elapsed().as_secs() < 60 {
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(25));
            cx.advance_clock(std::time::Duration::from_millis(25));
            let _ = cx.capture_screenshot(any);
            if busy(cx) { quiet = 0 } else { quiet += 1 }
        }
        cx.run_until_parked();
        eprintln!("settled after {:.2?}", t0.elapsed());
    };
    let with =
        |cx: &mut HeadlessAppContext,
         f: &dyn Fn(&mut Workspace, &mut gpui::Window, &mut gpui::Context<Workspace>)| {
            cx.update_window(any, |root, window, cx| {
                let view = root.downcast::<Workspace>().unwrap();
                view.update(cx, |this, cx| f(this, window, cx));
            })
            .unwrap();
        };
    settle(&mut cx);

    with(&mut cx, &|ws, window, cx| {
        ws.debug_apply_query(
            o.search.clone(),
            o.sort,
            o.filter.clone().map(|(c, t)| {
                (
                    c,
                    Cond::Text {
                        op: TextOp::Contains,
                        needle: t,
                    },
                )
            }),
            o.zoom,
            window,
            cx,
        )
    });
    settle(&mut cx);
    with(&mut cx, &|ws, window, cx| {
        let Some(sheet) = ws.sheet().cloned() else {
            return;
        };
        sheet.update(cx, |s, cx| {
            if let Some(r) = o.scroll {
                s.go_to(r, 0, cx);
            }
            if let Some((a, b)) = o.select {
                s.go_to(a.0, a.1, cx);
                s.select_range(a, b, cx);
            }
            if let Some(c) = o.menu {
                s.open_menu(c, window, cx);
            }
            if let Some((r, c)) = o.ctx {
                s.debug_context_menu(r, c, cx);
            }
            if let Some(t) = &o.edit {
                s.debug_edit(t.clone(), window, cx);
            }
        });
    });
    settle(&mut cx);

    if o.frames > 0 {
        let mut total = 0.;
        let mut worst: f64 = 0.;
        for i in 0..o.frames {
            let t = thread_cpu();
            with(&mut cx, &|ws, _, cx| {
                if let Some(s) = ws.sheet().cloned() {
                    s.update(cx, |s, cx| {
                        s.scroll_y += 37.0;
                        s.scroll_x = if i % 50 < 25 { 0. } else { 120. };
                        cx.notify();
                    });
                }
            });
            cx.run_until_parked();
            let _ = cx.capture_screenshot(any);
            let dt = thread_cpu() - t;
            total += dt;
            worst = worst.max(dt);
        }
        eprintln!(
            "scroll frames: avg_cpu_ms={:.3} worst_ms={:.3}",
            total / o.frames as f64 * 1e3,
            worst * 1e3
        );
    }
    let img = cx.capture_screenshot(any).expect("screenshot");
    img.save(&out).expect("save png");
    eprintln!("wrote {}", out.display());
    let _ = point(px(0.), px(0.));
}
