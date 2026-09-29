//! "Literary boids" — a Reynolds flocking simulation of glyph-agents.
//!
//! Ported verbatim (mechanic-for-mechanic) from a skunkworks `literary-boids`
//! experiment's `boid.rs` / `world.rs`. Two deliberate translations for the
//! browser:
//!
//! * randomness — the original used the `rand` crate; here every stochastic
//!   draw goes through [`js_sys::Math::random`] (no `rand` dependency), and
//! * colour — the original used `ratatui::style::Color`; here a boid's colour is
//!   a CSS colour string drawn straight onto a 2D canvas.
//!
//! The world is a 200×200 toroidal (wrap-around) plane. Each frame runs O(N²)
//! neighbour math for separation / alignment / cohesion, boids eat corpus
//! characters (adopting them as their glyph), reproduce when well-fed, and die
//! when starved.

use std::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Sub, SubAssign};

/// World width in simulation units.
pub const WORLD_W: f64 = 200.0;
/// World height in simulation units.
pub const WORLD_H: f64 = 200.0;
/// Number of food particles kept alive at any time.
const FOOD_COUNT: usize = 50;
/// Hard population cap — reproduction is skipped past this so the sim stays
/// smooth and the O(N²) neighbour loop stays affordable per frame.
const MAX_POP: usize = 400;

// ── Randomness helpers (all on js_sys::Math::random) ─────────────────────────

/// Uniform f64 in `[0, 1)`.
fn rand() -> f64 {
    js_sys::Math::random()
}

/// Uniform f64 in `[a, b)`.
fn gen_range(a: f64, b: f64) -> f64 {
    a + rand() * (b - a)
}

/// Bernoulli draw: `true` with probability `p`.
fn gen_bool(p: f64) -> bool {
    rand() < p
}

// ── Vec2 ─────────────────────────────────────────────────────────────────────

/// A 2D vector. `Copy` — passed and returned by value throughout the sim.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn zero() -> Self {
        Self { x: 0.0, y: 0.0 }
    }

    pub fn magnitude_squared(&self) -> f64 {
        self.x * self.x + self.y * self.y
    }

    pub fn magnitude(&self) -> f64 {
        self.magnitude_squared().sqrt()
    }

    /// Unit vector in the same direction, or zero if this vector is zero.
    pub fn normalize(&self) -> Self {
        let mag = self.magnitude();
        if mag == 0.0 {
            Self::zero()
        } else {
            *self * (1.0 / mag)
        }
    }

    /// Scale down to length `max` if longer than `max`; otherwise unchanged.
    pub fn limit(self, max: f64) -> Self {
        if self.magnitude_squared() > max * max {
            self.normalize() * max
        } else {
            self
        }
    }

    pub fn distance_squared(&self, other: Vec2) -> f64 {
        (*self - other).magnitude_squared()
    }

    pub fn distance(&self, other: Vec2) -> f64 {
        (*self - other).magnitude()
    }
}

impl Add for Vec2 {
    type Output = Vec2;
    fn add(self, rhs: Vec2) -> Vec2 {
        Vec2::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, rhs: Vec2) -> Vec2 {
        Vec2::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl AddAssign for Vec2 {
    fn add_assign(&mut self, rhs: Vec2) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}

impl SubAssign for Vec2 {
    fn sub_assign(&mut self, rhs: Vec2) {
        self.x -= rhs.x;
        self.y -= rhs.y;
    }
}

impl Mul<f64> for Vec2 {
    type Output = Vec2;
    fn mul(self, rhs: f64) -> Vec2 {
        Vec2::new(self.x * rhs, self.y * rhs)
    }
}

impl MulAssign<f64> for Vec2 {
    fn mul_assign(&mut self, rhs: f64) {
        self.x *= rhs;
        self.y *= rhs;
    }
}

impl Div<f64> for Vec2 {
    type Output = Vec2;
    fn div(self, rhs: f64) -> Vec2 {
        Vec2::new(self.x / rhs, self.y / rhs)
    }
}

impl DivAssign<f64> for Vec2 {
    fn div_assign(&mut self, rhs: f64) {
        self.x /= rhs;
        self.y /= rhs;
    }
}

// ── Dna ──────────────────────────────────────────────────────────────────────

/// A boid's heritable traits. Colour is a CSS colour string; glyph is the
/// character it currently renders as (starts as `'*'`, becomes eaten food).
#[derive(Clone, Debug, PartialEq)]
pub struct Dna {
    pub max_speed: f64,
    pub max_force: f64,
    pub view_radius: f64,
    pub separation_weight: f64,
    pub alignment_weight: f64,
    pub cohesion_weight: f64,
    pub color: String,
    pub glyph: char,
}

impl Dna {
    /// A fresh random genome. Base colour is off-white; glyph is `'*'`.
    pub fn random() -> Self {
        Self {
            max_speed: gen_range(0.5, 1.5),
            max_force: gen_range(0.02, 0.1),
            view_radius: gen_range(5.0, 15.0),
            separation_weight: gen_range(1.0, 2.0),
            alignment_weight: gen_range(0.8, 1.2),
            cohesion_weight: gen_range(0.8, 1.2),
            color: "#e6e6e6".to_string(),
            glyph: '*',
        }
    }
}

// ── Boid ─────────────────────────────────────────────────────────────────────

/// One flocking agent.
#[derive(Clone, Debug)]
pub struct Boid {
    pub position: Vec2,
    pub velocity: Vec2,
    pub acceleration: Vec2,
    pub dna: Dna,
    pub energy: f64,
}

impl Boid {
    /// Spawn at `(x, y)` heading in a random direction at `max_speed`.
    pub fn new(x: f64, y: f64) -> Self {
        let dna = Dna::random();
        let theta = rand() * std::f64::consts::TAU;
        let velocity = Vec2::new(theta.cos(), theta.sin()) * dna.max_speed;
        Self {
            position: Vec2::new(x, y),
            velocity,
            acceleration: Vec2::zero(),
            dna,
            energy: 100.0,
        }
    }

    /// Accumulate a steering force for this tick.
    pub fn apply_force(&mut self, force: Vec2) {
        self.acceleration += force;
    }

    /// Integrate one step and wrap toroidally, burning a little energy.
    pub fn update(&mut self, w: f64, h: f64) {
        self.velocity += self.acceleration;
        self.velocity = self.velocity.limit(self.dna.max_speed);
        self.position += self.velocity;
        self.acceleration = Vec2::zero();

        if self.position.x < 0.0 {
            self.position.x += w;
        }
        if self.position.x >= w {
            self.position.x -= w;
        }
        if self.position.y < 0.0 {
            self.position.y += h;
        }
        if self.position.y >= h {
            self.position.y -= h;
        }

        self.energy -= 0.05;
    }

    /// Steer toward `target` at cruising speed.
    fn seek(&self, target: Vec2) -> Vec2 {
        let desired = (target - self.position).normalize() * self.dna.max_speed - self.velocity;
        desired.limit(self.dna.max_force)
    }

    /// Push away from crowding neighbours (within half the view radius).
    fn separation(&self, boids: &[Boid]) -> Vec2 {
        let radius_sq = (self.dna.view_radius / 2.0).powi(2);
        let mut steer = Vec2::zero();
        let mut count = 0u32;
        for other in boids {
            let d2 = self.position.distance_squared(other.position);
            if d2 > 0.0 && d2 < radius_sq {
                steer += (self.position - other.position) / d2;
                count += 1;
            }
        }
        if count > 0 && steer.magnitude() > 0.0 {
            steer = steer.normalize() * self.dna.max_speed - self.velocity;
            steer.limit(self.dna.max_force)
        } else {
            Vec2::zero()
        }
    }

    /// Match the average heading of visible neighbours.
    fn alignment(&self, boids: &[Boid]) -> Vec2 {
        let view_sq = self.dna.view_radius * self.dna.view_radius;
        let mut sum = Vec2::zero();
        let mut count = 0u32;
        for other in boids {
            let d2 = self.position.distance_squared(other.position);
            if d2 > 0.0 && d2 < view_sq {
                sum += other.velocity;
                count += 1;
            }
        }
        if count > 0 {
            sum /= count as f64;
            if sum.magnitude() > 0.0 {
                sum = sum.normalize() * self.dna.max_speed - self.velocity;
                return sum.limit(self.dna.max_force);
            }
        }
        Vec2::zero()
    }

    /// Steer toward the centre of mass of visible neighbours.
    fn cohesion(&self, boids: &[Boid]) -> Vec2 {
        let view_sq = self.dna.view_radius * self.dna.view_radius;
        let mut sum = Vec2::zero();
        let mut count = 0u32;
        for other in boids {
            let d2 = self.position.distance_squared(other.position);
            if d2 > 0.0 && d2 < view_sq {
                sum += other.position;
                count += 1;
            }
        }
        if count > 0 {
            sum /= count as f64;
            self.seek(sum)
        } else {
            Vec2::zero()
        }
    }

    /// Weighted sum of the three Reynolds rules — the total flocking force.
    ///
    /// Iterates all boids three times: O(N²) per boid, O(N²) per frame.
    pub fn calculate_flocking_force(&self, boids: &[Boid]) -> Vec2 {
        let separation = self.separation(boids) * self.dna.separation_weight;
        let alignment = self.alignment(boids) * self.dna.alignment_weight;
        let cohesion = self.cohesion(boids) * self.dna.cohesion_weight;
        separation + alignment + cohesion
    }
}

// ── Food ─────────────────────────────────────────────────────────────────────

/// A single character of corpus, sitting on the plane waiting to be eaten.
#[derive(Clone, Debug)]
pub struct Food {
    pub position: Vec2,
    pub content: char,
}

// ── World ────────────────────────────────────────────────────────────────────

/// The whole simulation: the flock, the food, and the corpus feeding it.
pub struct World {
    pub boids: Vec<Boid>,
    pub food: Vec<Food>,
    pub width: f64,
    pub height: f64,
    /// Non-whitespace characters of the corpus, consumed cyclically as food.
    text_source: Vec<char>,
    text_index: usize,
    // Reusable per-frame buffers (avoid per-frame allocation).
    force_buffer: Vec<Vec2>,
    eaten_buffer: Vec<usize>,
    child_buffer: Vec<Boid>,
}

impl World {
    /// Build a world seeded with `initial_boids` boids at the centre, feeding on
    /// the non-whitespace characters of `text`.
    pub fn new(text: &str, initial_boids: usize) -> Self {
        let text_source: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
        let mut world = Self {
            boids: Vec::new(),
            food: Vec::new(),
            width: WORLD_W,
            height: WORLD_H,
            text_source,
            text_index: 0,
            force_buffer: Vec::new(),
            eaten_buffer: Vec::new(),
            child_buffer: Vec::new(),
        };
        let (cx, cy) = (world.width / 2.0, world.height / 2.0);
        for _ in 0..initial_boids {
            world.boids.push(Boid::new(cx, cy));
        }
        for _ in 0..FOOD_COUNT {
            world.spawn_food();
        }
        world
    }

    /// Emit the next corpus character as a food particle at a random location,
    /// wrapping the read cursor back to the start of the corpus at the end.
    fn spawn_food(&mut self) {
        if self.text_source.is_empty() {
            return;
        }
        if self.text_index >= self.text_source.len() {
            self.text_index = 0;
        }
        let content = self.text_source[self.text_index];
        self.food.push(Food {
            position: Vec2::new(gen_range(0.0, self.width), gen_range(0.0, self.height)),
            content,
        });
        self.text_index += 1;
    }

    /// Advance the simulation one tick.
    pub fn update(&mut self) {
        // (1) Compute every boid's flocking force against every other boid.
        let mut forces = std::mem::take(&mut self.force_buffer);
        forces.clear();
        for i in 0..self.boids.len() {
            forces.push(self.boids[i].calculate_flocking_force(&self.boids));
        }

        // (2) Apply forces and integrate.
        for (boid, &force) in self.boids.iter_mut().zip(forces.iter()) {
            boid.apply_force(force);
            boid.update(self.width, self.height);
        }
        self.force_buffer = forces;

        // (3) Eat: each boid takes the first food within distance 2.0.
        let mut eaten = std::mem::take(&mut self.eaten_buffer);
        eaten.clear();
        for boid in self.boids.iter_mut() {
            for (fi, food) in self.food.iter().enumerate() {
                if eaten.contains(&fi) {
                    continue;
                }
                if boid.position.distance(food.position) < 2.0 {
                    boid.energy += 20.0;
                    boid.dna.glyph = food.content;
                    Self::syntax_physics(&mut boid.dna, food.content);
                    eaten.push(fi);
                    break;
                }
            }
        }
        // Remove eaten food (swap_remove, high index first) and top back up.
        eaten.sort_unstable();
        eaten.dedup();
        for &fi in eaten.iter().rev() {
            self.food.swap_remove(fi);
        }
        let removed = eaten.len();
        for _ in 0..removed {
            self.spawn_food();
        }
        self.eaten_buffer = eaten;

        // (4) Reproduce: well-fed boids split, capped at MAX_POP.
        let mut children = std::mem::take(&mut self.child_buffer);
        children.clear();
        let parent_len = self.boids.len();
        for boid in self.boids.iter_mut() {
            if boid.energy > 150.0 && parent_len + children.len() < MAX_POP {
                boid.energy -= 80.0;
                let mut child = boid.clone();
                child.energy = 80.0;
                Self::mutate_dna(&mut child.dna);
                children.push(child);
            }
        }
        self.boids.append(&mut children);
        self.child_buffer = children; // now drained/empty; keep the allocation

        // (5) Die: starved boids are removed; extinction guard reseeds 10.
        self.boids.retain(|b| b.energy > 0.0);
        if self.boids.is_empty() {
            let (cx, cy) = (self.width / 2.0, self.height / 2.0);
            for _ in 0..10 {
                self.boids.push(Boid::new(cx, cy));
            }
        }
    }

    /// Random drift + colour reclassification applied to a newborn's genome.
    fn mutate_dna(dna: &mut Dna) {
        if gen_bool(0.1) {
            dna.max_speed = (dna.max_speed + gen_range(-0.1, 0.1)).clamp(0.2, 2.0);
        }
        if gen_bool(0.1) {
            dna.view_radius = (dna.view_radius + gen_range(-1.0, 1.0)).clamp(2.0, 20.0);
        }
        dna.color = if dna.max_speed > 1.0 {
            "#ff5c57" // fast → red
        } else if dna.view_radius > 10.0 {
            "#57c7ff" // far-sighted → blue
        } else {
            "#5af78e" // otherwise → green
        }
        .to_string();
    }

    /// "Syntax physics": eating a character nudges the boid's genome based on
    /// what kind of character it is — punctuation makes it skittish, digits make
    /// it march in step, vowels widen its view. Keeps code-shaped flocks lively.
    fn syntax_physics(dna: &mut Dna, ch: char) {
        if ch.is_uppercase() {
            dna.max_speed *= 1.1;
        } else if ch.is_ascii_digit() {
            dna.alignment_weight *= 1.2;
        } else if ch.is_ascii_punctuation() || ch.is_whitespace() {
            dna.separation_weight *= 1.1;
            dna.cohesion_weight *= 0.9;
        } else if matches!(ch.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u') {
            dna.view_radius *= 1.05;
        } else {
            dna.cohesion_weight *= 1.05;
        }
        dna.max_speed = dna.max_speed.clamp(0.2, 2.0);
        dna.view_radius = dna.view_radius.clamp(2.0, 20.0);
        dna.separation_weight = dna.separation_weight.clamp(0.5, 3.0);
        dna.alignment_weight = dna.alignment_weight.clamp(0.5, 3.0);
        dna.cohesion_weight = dna.cohesion_weight.clamp(0.5, 3.0);
    }
}
