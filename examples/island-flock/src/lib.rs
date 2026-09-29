//! A Yew CSR "island": a self-contained interactive widget that maud's
//! server-rendered page mounts into an existing DOM element.
//!
//! This one is **"literary boids"** — a Reynolds flocking simulation where each
//! glyph-agent becomes the last character of Autumn's own source code it eats.
//! Everything (the O(N²) neighbour math, the ecosystem, the canvas drawing)
//! runs entirely client-side in WebAssembly; there are no server round-trips.
//!
//! This is a SPIKE / recipe, not a framework subsystem. See
//! `docs/guide/wasm-islands.md`. The whole boundary is a plain DOM element plus
//! serialized props (data attributes) — there is no SSR and no hydration, so
//! yew's `html!` never collides with maud's `html!` (they live in different
//! crates compiled for different targets).
//!
//! Build with `./build-island.sh` (wraps `cargo build --target
//! wasm32-unknown-unknown --release` + `wasm-bindgen --target web`).

mod corpus;
mod sim;

use std::cell::RefCell;
use std::rc::Rc;

use gloo_render::{AnimationFrame, request_animation_frame};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::wasm_bindgen;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};
use yew::prelude::*;

use corpus::AUTUMN_SOURCE;
use sim::{WORLD_H, WORLD_W, World};

/// Logical canvas size in device pixels.
const CANVAS_W: u32 = 840;
const CANVAS_H: u32 = 560;
/// Target sim cadence in milliseconds (~33 fps).
const FRAME_MS: f64 = 30.0;

/// Props for [`Flock`]. `count` is the initial boid population, supplied by the
/// server via a `data-count` attribute on the mount element.
#[derive(Properties, PartialEq)]
pub struct FlockProps {
    /// Initial number of boids to spawn.
    pub count: u32,
}

/// Draw one frame: dark background, dim-green food glyphs, coloured boid glyphs.
///
/// World coordinates `[0, 200]²` map onto the canvas by a per-axis scale, with
/// the Y axis flipped (canvas Y grows downward, the sim's grows upward).
fn draw(ctx: &CanvasRenderingContext2d, world: &World, scale_x: f64, scale_y: f64) {
    let w = f64::from(CANVAS_W);
    let h = f64::from(CANVAS_H);

    ctx.set_fill_style_str("#12131a");
    ctx.fill_rect(0.0, 0.0, w, h);
    ctx.set_font("14px monospace");

    // Food: dim green.
    ctx.set_fill_style_str("#3a6b4a");
    let mut buf = [0u8; 4];
    for f in &world.food {
        let px = f.position.x * scale_x;
        let py = h - f.position.y * scale_y;
        let _ = ctx.fill_text(f.content.encode_utf8(&mut buf), px, py);
    }

    // Boids: each in its own DNA colour.
    for b in &world.boids {
        ctx.set_fill_style_str(&b.dna.color);
        let px = b.position.x * scale_x;
        let py = h - b.position.y * scale_y;
        let _ = ctx.fill_text(b.dna.glyph.encode_utf8(&mut buf), px, py);
    }
}

/// The literary-boids island. maud owns the page; this component owns only the
/// `<canvas>` inside its mount element plus a small control strip.
#[function_component(Flock)]
pub fn flock(props: &FlockProps) -> Html {
    let canvas_ref = use_node_ref();
    let paused = use_state(|| false);
    let boid_count = use_state(|| 0usize);
    let food_count = use_state(|| 0usize);
    // Bumped by "Reseed" to restart the animation with a fresh World.
    let reseed_gen = use_state(|| 0u32);

    // Shared pause flag the rAF loop reads each frame without re-subscribing.
    let paused_flag = use_mut_ref(|| false);
    *paused_flag.borrow_mut() = *paused;

    {
        let canvas_ref = canvas_ref.clone();
        let count = props.count;
        let paused_flag = paused_flag.clone();
        let boid_count = boid_count.clone();
        let food_count = food_count.clone();

        // Restart the whole animation whenever the reseed generation (or count)
        // changes. First render seeds the initial World.
        use_effect_with((*reseed_gen, count), move |_| {
            // Live rAF handle; the self-rescheduling tick closure keeps itself
            // alive via `tick`. Dropping both on cleanup cancels the loop.
            let handle: Rc<RefCell<Option<AnimationFrame>>> = Rc::new(RefCell::new(None));
            #[allow(clippy::type_complexity)]
            let tick: Rc<RefCell<Option<Box<dyn Fn(f64)>>>> = Rc::new(RefCell::new(None));

            if let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() {
                let ctx = canvas
                    .get_context("2d")
                    .ok()
                    .flatten()
                    .and_then(|c| c.dyn_into::<CanvasRenderingContext2d>().ok());

                if let Some(ctx) = ctx {
                    let scale_x = f64::from(CANVAS_W) / WORLD_W;
                    let scale_y = f64::from(CANVAS_H) / WORLD_H;
                    let world = Rc::new(RefCell::new(World::new(AUTUMN_SOURCE, count as usize)));
                    let last = Rc::new(RefCell::new(0.0_f64));

                    // Build the self-rescheduling frame callback.
                    let tick_ref = tick.clone();
                    let handle_ref = handle.clone();
                    *tick.borrow_mut() = Some(Box::new(move |time: f64| {
                        let due = {
                            let mut l = last.borrow_mut();
                            if time - *l >= FRAME_MS {
                                *l = time;
                                true
                            } else {
                                false
                            }
                        };
                        if due {
                            let mut w = world.borrow_mut();
                            if !*paused_flag.borrow() {
                                w.update();
                            }
                            // Draw every due frame (even when paused) so the
                            // canvas stays painted after a re-render.
                            draw(&ctx, &w, scale_x, scale_y);
                            boid_count.set(w.boids.len());
                            food_count.set(w.food.len());
                        }
                        // Reschedule the next animation frame.
                        let t = tick_ref.clone();
                        *handle_ref.borrow_mut() = Some(request_animation_frame(move |time| {
                            if let Some(f) = t.borrow().as_ref() {
                                f(time);
                            }
                        }));
                    }));

                    // Kick off the loop.
                    let t = tick.clone();
                    *handle.borrow_mut() = Some(request_animation_frame(move |time| {
                        if let Some(f) = t.borrow().as_ref() {
                            f(time);
                        }
                    }));
                }
            }

            // Cleanup: cancel the pending frame and break the tick's self-cycle.
            move || {
                handle.borrow_mut().take();
                tick.borrow_mut().take();
            }
        });
    }

    let on_pause = {
        let paused = paused.clone();
        Callback::from(move |_| paused.set(!*paused))
    };
    let on_reseed = {
        let reseed_gen = reseed_gen.clone();
        Callback::from(move |_| reseed_gen.set(*reseed_gen + 1))
    };

    let pause_label = if *paused { "Resume" } else { "Pause" };

    html! {
        <div class="flock-island">
            <div class="flock-controls">
                <button data-testid="pause" onclick={on_pause}>{ pause_label }</button>
                <button data-testid="reseed" onclick={on_reseed}>{ "Reseed" }</button>
                <span data-testid="readout" class="flock-readout">
                    { format!("boids: {} · food: {} · corpus: Autumn source", *boid_count, *food_count) }
                </span>
            </div>
            <canvas
                id="flock-canvas"
                ref={canvas_ref}
                width={CANVAS_W.to_string()}
                height={CANVAS_H.to_string()}
                style="width:100%;max-width:840px;height:auto;border-radius:8px;background:#12131a;display:block"
            />
        </div>
    }
}

/// WASM entry point. Called from `flock-boot.js` after `init()`.
///
/// Renders a fresh [`Flock`] into the supplied mount element via yew's
/// `with_root_and_props` — the clean "mount into someone else's element"
/// primitive that makes this island architecture work without yew owning the
/// page. The loader resolves the element and passes it in directly; `count` is
/// the initial boid population read from the `data-count` attribute.
#[wasm_bindgen]
pub fn mount(el: web_sys::Element, count: u32) {
    yew::Renderer::<Flock>::with_root_and_props(el, FlockProps { count }).render();
}
