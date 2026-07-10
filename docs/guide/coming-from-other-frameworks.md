# Coming From Other Frameworks

If you think in Spring Boot, Django, or Rails, this guide maps the concepts
you already know to their Autumn equivalents. Same ideas, different syntax.

---

## The 30-Second Version

| You know...         | In Autumn it's...                          |
|---------------------|--------------------------------------------|
| Controller          | A module with `#[get]`/`#[post]` functions |
| Service / Bean      | `#[service]` trait                         |
| Repository / DAO    | `#[repository(Model)]` trait               |
| Model / Entity      | `#[model]` struct                          |
| Dependency injection| Axum extractors (auto-wired from handler params) |
| `application.yml`   | `autumn.toml` + `AUTUMN_*` env vars        |
| Migrations          | Diesel migrations (`diesel migration generate`) |
| Middleware / Filter  | Tower layers + `#[intercept]`              |
| Template engine     | Maud (compile-time HTML macros)            |
| ORM queries         | Diesel query builder                       |

---

## Coming From Spring Boot

### Controllers

**Spring Boot:**

```java
@RestController
@RequestMapping("/api/posts")
public class PostController {
    @Autowired
    private PostService postService;

    @GetMapping
    public List<Post> list() {
        return postService.findAll();
    }

    @GetMapping("/{id}")
    public Post getById(@PathVariable Long id) {
        return postService.findById(id)
            .orElseThrow(() -> new ResponseStatusException(HttpStatus.NOT_FOUND));
    }

    @PostMapping
    @ResponseStatus(HttpStatus.CREATED)
    public Post create(@Valid @RequestBody NewPostDto dto) {
        return postService.create(dto);
    }
}
```

**Autumn:**

```rust
use autumn_web::prelude::*;

// No controller class -- just functions in a module (e.g., src/routes/posts.rs)

#[get("/api/posts")]
async fn list(repo: PgPostRepository) -> AutumnResult<Json<Vec<Post>>> {
    Ok(Json(repo.find_all().await?))
}

#[get("/api/posts/{id}")]
async fn get_by_id(Path(id): Path<i64>, repo: PgPostRepository) -> AutumnResult<Json<Post>> {
    Ok(Json(repo.find_by_id(id).await?))  // 404 if not found
}

#[post("/api/posts")]
async fn create(
    repo: PgPostRepository,
    Valid(Json(dto)): Valid<Json<NewPost>>,
) -> AutumnResult<Json<Post>> {
    Ok(Json(repo.save(&dto).await?))
}
```

Key differences:
- No class, no `@Autowired`. Dependencies are handler parameters -- Autumn
  extracts them automatically.
- Validation via `Valid<Json<T>>` instead of `@Valid @RequestBody`.
- Error handling via `?` and `AutumnResult` instead of exceptions.

### Services and Dependency Injection

**Spring Boot:**

```java
@Service
public class OrderService {
    @Autowired private OrderRepository orderRepo;
    @Autowired private InventoryService inventory;

    public Order placeOrder(OrderRequest req) {
        Order order = orderRepo.save(new Order(req));
        inventory.reserve(order.getId());
        return order;
    }
}
```

**Autumn:**

```rust
#[service]
pub trait OrderService {
    fn deps(order_repo: PgOrderRepository, inventory: InventoryServiceImpl);
    async fn place_order(&self, req: OrderRequest) -> AutumnResult<Order>;
}

impl OrderServiceImpl {
    pub async fn place_order(&self, req: OrderRequest) -> AutumnResult<Order> {
        let order = self.order_repo.save(&req.into()).await?;
        self.inventory.reserve(order.id).await?;
        Ok(order)
    }
}

// In a handler -- just add it as a parameter:
#[post("/orders")]
async fn create_order(svc: OrderServiceImpl, Json(req): Json<OrderRequest>)
    -> AutumnResult<Json<Order>>
{
    Ok(Json(svc.place_order(req).await?))
}
```

Spring's `@Autowired` scans the classpath and creates beans at startup.
Autumn's approach is per-request extraction: each handler parameter is
resolved from the request and app state. No startup scanning, no bean
lifecycle, no circular dependency issues.

### Repositories

**Spring Data JPA:**

```java
@Repository
public interface PostRepository extends JpaRepository<Post, Long> {
    List<Post> findByPublished(boolean published);
    long countByAuthorId(Long authorId);
}
```

**Autumn:**

```rust
#[repository(Post)]
pub trait PostRepository {
    fn find_by_published(published: bool) -> Vec<Post>;
    fn count_by_author_id(author_id: i64) -> i64;
}
```

This is the closest 1:1 mapping in the framework. Autumn parses method names
the same way Spring Data does: `find_by_X_and_Y`, `count_by_X`, `exists_by_X`,
`delete_by_X`. It generates the SQL queries at compile time via Diesel.

### Configuration and Profiles

**Spring Boot:**

```yaml
# application.yml
spring:
  profiles:
    active: dev
  datasource:
    url: jdbc:postgresql://localhost/mydb
server:
  port: 8080
```

**Autumn:**

```toml
# autumn.toml
[server]
port = 8080

[database]
url = "postgres://localhost/mydb"
```

| Spring                                  | Autumn                              |
|-----------------------------------------|-------------------------------------|
| `application.yml`                       | `autumn.toml`                       |
| `application-dev.yml`                   | `autumn-dev.toml`                   |
| `application-prod.yml`                  | `autumn-prod.toml`                  |
| `SPRING_DATASOURCE_URL`                 | `AUTUMN_DATABASE__URL`              |
| `@Value("${server.port}")`              | `config.server.port`                |
| `spring.profiles.active`               | `AUTUMN_PROFILE` or auto-detect     |

Profile smart defaults are built in. Dev gives you pretty logging, permissive
CORS, and fast shutdown. Prod gives you JSON logging, strict CORS, and HSTS.
No `application-dev.yml` required for the common case.

### Security

**Spring Security:**

```java
@PreAuthorize("hasRole('ADMIN')")
@GetMapping("/admin")
public String adminPanel() { return "welcome"; }
```

**Autumn:**

```rust
#[get("/admin")]
#[secured("admin")]
async fn admin_panel() -> &'static str {
    "welcome"
}
```

### Record-level authorization

Spring Security's `@PreAuthorize` / `@PostAuthorize`, Rails Pundit,
Phoenix Bodyguard, Django `has_object_permission`, and Rails
`before_action` all answer the same question: "is this user allowed to
act on *this specific record*?" Autumn's `Policy` trait + `#[authorize]`
macro is the trait-plugin idiom for the same surface.

| Framework            | Record-level authz idiom                                                  | Autumn equivalent                                                                       |
|----------------------|---------------------------------------------------------------------------|-----------------------------------------------------------------------------------------|
| Spring               | `@PreAuthorize("...")` / `@PostAuthorize`                                 | `#[authorize("update", resource = Post)]`                                               |
| Rails                | `Pundit` — `authorize @post`, `policy_scope(Post)`, `before_action`       | `#[authorize(...)]` + `Scope` trait + `Policy::register_*`                              |
| Phoenix              | `Bodyguard.permit(MyApp.Blog, :update_post, user, post)`                  | `autumn_web::authorization::authorize::<Post>(...)` (inline) or `#[authorize]`          |
| Django               | `has_object_permission` (DRF) / `django-guardian`                         | `Policy::can_show / can_update / can_delete`                                            |
| Loco.rs / axum / actix-web / rocket | Hand-rolled `if record.author_id != user_id` everywhere    | Single `Policy<R>` impl + `.policy::<R, _>(...)` registration                           |

See [`docs/guide/authorization.md`](./authorization.md) for the full
walkthrough including scope queries, `[security] forbidden_response`,
and the reddit-clone migration.

### Actuator

Both frameworks provide actuator endpoints out of the box:

| Spring Actuator          | Autumn Actuator                |
|--------------------------|--------------------------------|
| `/actuator/health`       | `/actuator/health`             |
| `/actuator/info`         | `/actuator/info`               |
| `/actuator/metrics`      | `/actuator/metrics`            |
| `/actuator/env`          | `/actuator/configprops`        |
| `/actuator/loggers`      | `/actuator/loggers`            |
| `/actuator/scheduledtasks` | `/actuator/scheduledtasks`   |

Like Spring Boot's `loggers` actuator endpoint, Autumn's logger levels reload
live -- `LogLevels::set_logger_level(name, level)` flips a target's level at
runtime (with `current_level()` / `logger_overrides()` to inspect), no restart
required. And just as `/actuator/info` surfaces git/build info, Autumn's
`/info` carries a `BuildProvenance` (git SHA plus build metadata) via
`build_provenance()`.

---

## Coming From Django

### Views / URL routing

**Django:**

```python
# urls.py
urlpatterns = [
    path('posts/', views.list_posts),
    path('posts/<int:pk>/', views.get_post),
]

# views.py
def list_posts(request):
    posts = Post.objects.all()
    return JsonResponse({'posts': list(posts.values())})

def get_post(request, pk):
    post = get_object_or_404(Post, pk=pk)
    return JsonResponse(model_to_dict(post))
```

**Autumn:**

```rust
// Route registration -- similar to urls.py
autumn_web::app()
    .routes(routes![list_posts, get_post])
    .run()
    .await;

// Handlers -- similar to views.py
#[get("/posts")]
async fn list_posts(mut db: Db) -> AutumnResult<Json<Vec<Post>>> {
    let posts = posts::table.load(&mut *db).await?;
    Ok(Json(posts))
}

#[get("/posts/{id}")]
async fn get_post(Path(id): Path<i32>, mut db: Db) -> AutumnResult<Json<Post>> {
    let post = posts::table.find(id).first(&mut *db).await
        .map_err(AutumnError::not_found)?;  // like get_object_or_404
    Ok(Json(post))
}
```

### Models and Migrations

**Django:**

```python
class Post(models.Model):
    title = models.CharField(max_length=200)
    body = models.TextField()
    published = models.BooleanField(default=False)
    created_at = models.DateTimeField(auto_now_add=True)
```

**Autumn:**

```rust
#[model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: String,
    #[default]
    pub published: bool,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}
```

| Django                   | Autumn                                |
|--------------------------|---------------------------------------|
| `python manage.py makemigrations` | `diesel migration generate create_posts` |
| `python manage.py migrate` | `diesel migration run` (or auto at startup) |
| `Model.objects.all()`   | `posts::table.load(&mut *db).await`   |
| `Model.objects.filter()` | `posts::table.filter(...).load()`    |
| `Model.objects.get(pk=1)` | `posts::table.find(1).first()`      |
| `get_object_or_404()`   | `.map_err(AutumnError::not_found)?`   |
| `model.save()`          | `diesel::insert_into(...).values(...)` |
| `ModelSerializer`       | `#[derive(Serialize, Deserialize)]` (via serde) |
| `.iterator()` / chunking | `repo.find_each(500)` / `repo.find_in_batches(1000)` (keyset) |
| `get_or_create()`       | `repo.find_or_create_by_<field>().await` → `(Model, bool)` |
| `.values(...).annotate(Count(...))` | `repo.count_grouped_by_<col>().load().await` |

`find_each` / `find_in_batches` stream rows in keyset (PK cursor) batches --
the Django `.iterator()` / chunking pattern without loading everything into
memory. `find_or_create_by_<field>` is the race-safe `get_or_create()`
equivalent, returning `(Model, created)`. Grouped aggregates
(`count_grouped_by_x`, `sum_y_grouped_by_x`, plus avg/min/max) map to
`.values(...).annotate(Count/Sum)` and return a builder with `.filter_eq`,
`.filter_range`, and `.bucket`.

Django generates migrations from model changes. In Autumn, you write SQL
migrations by hand (or use `diesel migration generate`). The tradeoff: more
control over SQL, less magic.

### Settings

**Django:**

```python
# settings.py
DATABASES = {
    'default': {
        'ENGINE': 'django.db.backends.postgresql',
        'NAME': 'mydb',
    }
}
DEBUG = True
```

**Autumn:**

```toml
# autumn.toml
[database]
url = "postgres://localhost/mydb"

[log]
level = "debug"
```

| Django                          | Autumn                           |
|---------------------------------|----------------------------------|
| `settings.py`                   | `autumn.toml`                    |
| `settings_dev.py`               | `autumn-dev.toml`                |
| `os.environ.get('DB_URL')`      | `AUTUMN_DATABASE__URL`           |
| `DEBUG = True`                  | Auto (`dev` profile in debug builds) |
| Middleware list in `settings.py` | Tower layers, `#[intercept]`    |

Where Django reaches for `django-environ` or `python-dotenv`, Autumn
auto-loads a `.env` file in the `dev` and `test` profiles (real OS env always
wins), and `autumn new` scaffolds a `.env.example` to document the keys your
app expects.

### Templates

**Django:**

```html
{% extends "base.html" %}
{% block content %}
  <h1>{{ post.title }}</h1>
  <p>{{ post.body }}</p>
{% endblock %}
```

**Autumn (Maud):**

```rust
fn layout(title: &str, content: Markup) -> Markup {
    html! {
        html { head { title { (title) } } body { (content) } }
    }
}

#[get("/posts/{id}")]
async fn show_post(Path(id): Path<i64>, mut db: Db) -> AutumnResult<Markup> {
    let post = posts::table.find(id).first(&mut *db).await
        .map_err(AutumnError::not_found)?;
    Ok(layout(&post.title, html! {
        h1 { (&post.title) }
        p { (&post.body) }
    }))
}
```

Maud templates are Rust code -- compile-time checked, no template file
mismatches, and full IDE support. The tradeoff: no separate template files
(designers can't edit them independently).

---

## Coming From Rails

### Controllers and Routes

**Rails:**

```ruby
# config/routes.rb
resources :posts

# app/controllers/posts_controller.rb
class PostsController < ApplicationController
  def index
    @posts = Post.all
    render json: @posts
  end

  def show
    @post = Post.find(params[:id])
    render json: @post
  end

  def create
    @post = Post.create!(post_params)
    render json: @post, status: :created
  end

  private
  def post_params
    params.require(:post).permit(:title, :body)
  end
end
```

**Autumn:**

```rust
// src/routes/posts.rs

#[get("/posts")]
async fn index(repo: PgPostRepository) -> AutumnResult<Json<Vec<Post>>> {
    Ok(Json(repo.find_all().await?))
}

#[get("/posts/{id}")]
async fn show(Path(id): Path<i64>, repo: PgPostRepository) -> AutumnResult<Json<Post>> {
    Ok(Json(repo.find_by_id(id).await?))
}

#[post("/posts")]
async fn create(
    repo: PgPostRepository,
    Valid(Json(params)): Valid<Json<NewPost>>,
) -> AutumnResult<Json<Post>> {
    Ok(Json(repo.save(&params).await?))
}

// In main.rs
autumn_web::app()
    .routes(routes![posts::index, posts::show, posts::create])
    .run()
    .await;
```

No `resources :posts` shorthand yet. You declare each route explicitly. The
`#[repository]` macro gives you the CRUD methods, but you wire routes
manually.

For the view side, `form_for(&changeset, "/posts", "post").csrf(tok).render()`
mirrors Rails' `form_for` / `form_with` -- a changeset-backed form builder that
renders fields (and errors) bound to your model.

### Active Record vs. Diesel

| Rails (Active Record)        | Autumn (Diesel)                          |
|------------------------------|------------------------------------------|
| `Post.all`                   | `posts::table.load(&mut *db).await`      |
| `Post.find(1)`              | `posts::table.find(1).first(&mut *db).await` |
| `Post.where(published: true)` | `posts::table.filter(posts::published.eq(true))` |
| `Post.create!(attrs)`       | `diesel::insert_into(posts::table).values(&new_post)` |
| `post.update!(title: "new")` | `diesel::update(posts::table.find(id)).set(...)` |
| `post.destroy`              | `diesel::delete(posts::table.find(id))` |
| `Post.count`                | `posts::table.count().get_result(&mut *db)` |
| `Post.find_each` / `find_in_batches` | `repo.find_each(500)` / `repo.find_in_batches(1000)` (exact namesakes) |
| `Post.find_or_create_by(...)` | `repo.find_or_create_by_<field>().await` → `(Model, bool)` |
| `Post.group(:x).count` / `.sum` | `repo.count_grouped_by_x()` / `repo.sum_y_grouped_by_x()` |
| Callbacks (`before_save`)   | Mutation hooks (`#[repository(Post, hooks = MyHooks)]`) |

Or use the `#[repository]` macro for a higher-level API:

```rust
repo.find_all().await           // Post.all
repo.find_by_id(1).await        // Post.find(1)
repo.save(&new_post).await      // Post.create!(attrs)
repo.update(1, &changes).await  // post.update!(attrs)
repo.delete_by_id(1).await      // post.destroy
repo.count().await               // Post.count
```

A few of these are exact namesakes of Active Record: `find_each` and
`find_in_batches` batch by keyset (PK cursor) just like Rails, and
`find_or_create_by_<field>` is the race-safe twin of `find_or_create_by`
(returning `(Model, created)`). Grouped aggregates -- `count_grouped_by_x`,
`sum_y_grouped_by_x` -- stand in for `group(:x).count` / `.sum`.

### Migrations

**Rails:**

```bash
rails generate migration CreatePosts title:string body:text
rails db:migrate
```

**Autumn:**

```bash
diesel migration generate create_posts
# Edit up.sql and down.sql by hand
diesel migration run
```

Rails generates migration content from the command line. Diesel generates empty
`up.sql`/`down.sql` files that you fill in with SQL. More verbose, but you
have full control over the SQL.

### Before/After Filters

**Rails:**

```ruby
class ApplicationController < ActionController::Base
  before_action :authenticate_user!
end

class AdminController < ApplicationController
  before_action :require_admin
end
```

**Autumn:**

```rust
// Per-handler authentication
#[get("/admin")]
#[secured("admin")]
async fn admin_panel() -> &'static str { "welcome" }

// Per-group middleware
autumn_web::app()
    .scoped("/admin", AuthLayer::new(), routes![admin_panel, admin_settings])
    .run()
    .await;
```

### Background Jobs

**Rails (Sidekiq/ActiveJob):**

```ruby
class CleanupJob < ApplicationJob
  def perform
    Post.where('created_at < ?', 30.days.ago).destroy_all
  end
end

# Scheduled via sidekiq-cron
CleanupJob.perform_later
```

**Autumn:**

```rust
#[scheduled(every = "24h", name = "cleanup")]
async fn cleanup(state: AppState) -> AutumnResult<()> {
    let mut db = state.db().await?;
    diesel::delete(posts::table.filter(
        posts::created_at.lt(chrono::Utc::now().naive_utc() - chrono::Duration::days(30))
    )).execute(&mut *db).await?;
    Ok(())
}

// Register in main:
autumn_web::app()
    .tasks(tasks![cleanup])
    .run()
    .await;
```

No Redis or external job queue is needed for simple scheduled tasks. For
durable request-triggered background work with retries, use Autumn's `#[job]`
runtime.

Testing enqueues feels familiar too: the test client's `assert_job_enqueued`
and `assert_job_enqueued_with(name, json)` mirror ActiveJob's
`assert_enqueued_with` (with `perform_enqueued_jobs().await` to drain them).
And where Rails 7.2 reaches for `rate_limit` (or rack-attack), Autumn's
`#[throttle(limit = 5, per = "1m", key = "ip")]` applies per-route rate
limiting -- placed outermost, above `#[get]` / `#[post]`.

For Temporal, Celery canvas, Sidekiq batches, Spring Batch, or Camunda-style
orchestration, use Autumn Harvest. Harvest is the companion workflow engine for
workflow history, long-running activities, timers, and singleton execution
across replicas. It depends on Autumn Web integration points, so it stays on its
own release train instead of being required by core web examples.

### Convention vs. Configuration

| Convention            | Rails                      | Autumn                        |
|-----------------------|----------------------------|-------------------------------|
| Table naming          | `Post` → `posts`           | `Post` → `posts` (same)      |
| Insert struct naming  | N/A (same model)           | `Post` → `NewPost`           |
| Update struct naming  | N/A (same model)           | `Post` → `UpdatePost`        |
| Repo struct naming    | N/A (Active Record)        | `Post` → `PgPostRepository`  |
| Service struct naming | N/A                        | `OrderService` → `OrderServiceImpl` |
| Config file           | `config/database.yml`      | `autumn.toml`                 |
| Profile config        | `config/environments/`     | `autumn-{profile}.toml`       |

The CLI keeps the Rails muscle memory, too: `autumn test` (with `--reset`)
spins up an isolated test DB and runs the suite like `rails test` /
`rails db:test:prepare`, `autumn destroy` is the exact namesake of
`rails destroy` for reverting a generator, and `autumn i18n check` gives you
an i18n-tasks-style health report on missing and unused translation keys.

---

## Recently Added: Cross-Framework Feature Map

The features below landed in the latest release wave; this table maps the idiom
you'd search for in each framework to the Autumn API that covers it. (A few
teased items are *not* in this wave yet: a charts widget, password-policy +
remember-me, and a query-count test assertion -- only a dev-mode N+1 inspector,
`detect_n_plus_one`, exists today.)

| Area        | Autumn                                                                              | Rails                                | Django                               | Laravel                              | Phoenix                              |
|-------------|-------------------------------------------------------------------------------------|--------------------------------------|--------------------------------------|--------------------------------------|--------------------------------------|
| Data        | `find_each`, `find_in_batches` (repo methods, keyset)                               | `find_each`, `find_in_batches`       | `.iterator()`                        | `chunk()`, `cursor()`                | `Repo.stream`                        |
| Data        | `find_or_create_by_<field>` → `(Model, bool)`                                        | `find_or_create_by`                  | `get_or_create()`                    | `firstOrCreate()`                    | `Repo` upsert (`on_conflict`)        |
| Data        | grouped aggregates: `count_grouped_by_x`, `sum_y_grouped_by_x`                       | `group(:x).count` / `.sum`           | `.values(...).annotate(Count/Sum)`   | `groupBy()->selectRaw`               | `Repo.aggregate` / `group_by`        |
| Data        | `#[state_machine(transitions(...))]` (on a `#[model]` String field)                  | AASM / Statesman gems                | `django-fsm`                         | `spatie/laravel-model-states`        | `Machinery`                          |
| Data        | audit actor attribution (`VersionEntry.actor`, `Model::history(...)`)                | paper_trail (`whodunnit`)            | django-simple-history                | `owen-it/laravel-auditing`           | `ex_audit`                           |
| Web         | `form_for(&changeset, action, method)`                                               | `form_for`, `form_with`              | `ModelForm`                          | `Form::model()` (LaravelCollective)  | `<.form for={@form}>`                |
| Web         | `Download` + HTTP Range/206 (`Download::from_bytes(...)`, honors `Range`)            | `send_file`, `send_data`             | `FileResponse` (Range)               | `response()->download()`             | `send_download`                      |
| Web         | `cache_for(Duration)` → `CacheControl` (defaults `private`)                          | `expires_in`, `fresh_when`           | `cache_control` decorator            | `Cache-Control` header               | `Plug.Conn` cache headers            |
| Web         | `Feed::atom(...)`, `Feed::rss(...)` (impl `IntoResponse`)                            | `atom_feed` builder                  | `django.contrib.syndication`         | `spatie/laravel-feed`                | —                                    |
| Web         | Server-Timing header (`[observability] server_timing = true`)                        | rack-mini-profiler (manual)          | manual                               | manual                               | manual                               |
| Realtime    | resumable SSE via Last-Event-ID (`sse::stream_resumable(...)`, `LastEventId`)        | `ActionController::Live`             | channels SSE                         | Broadcasting / SSE                   | Phoenix Channels / LiveView          |
| Mail        | suppression list (`SuppressionStore`, `MailBuilder::ignore_suppression()`)           | provider-side                        | `django-post-office`                 | provider-side                        | Swoosh / Bamboo                      |
| Mail        | CSS inlining at send (`MailBuilder::inline_css(true)`)                               | premailer-rails                      | Roadie                               | premailer                            | premailer                            |
| Concurrency | distributed advisory `Lock` (`Lock::new(...).try_lock()` / `.lock()`)               | `with_advisory_lock` gem             | `select_for_update` / advisory       | `Cache::lock()`                      | `:global` / Postgrex advisory        |
| Config      | `.env` auto-load in dev/test (+ `.env.example`)                                      | `dotenv-rails`                       | `django-environ`                     | built-in `.env` (phpdotenv)          | `Dotenvy`                            |
| Security    | `#[throttle(limit = 5, per = "1m", key = "ip")]`                                     | rack-attack, `rate_limit`            | `django-ratelimit`, DRF throttling   | `throttle:60,1` middleware           | `PlugAttack` / Hammer                |
| UI          | htmx `toast()` / `toast_region()` + `infinite_feed()` / `feed_page()` widgets        | Turbo Streams + flash                | messages framework                   | Livewire / Blade                     | LiveView streams (`phx-update`)      |
| Testing     | `TestClient::acting_as(user_id)` (+ `login_as`, `log_out`), cookie-jar client        | `sign_in` (Devise)                   | `force_login()`                      | `actingAs()` (exact namesake)        | `log_in_user` conn helper            |
| Testing     | job recorder asserts (`assert_job_enqueued`, `assert_job_enqueued_with`)             | `assert_enqueued_with` (ActiveJob)   | —                                    | `Queue::fake()`, `Bus::fake()`       | `Oban.Testing` `assert_enqueued`     |
| CLI         | `autumn test [--reset]` (isolated test DB + run suite)                               | `rails test`, `rails db:test:prepare`| `manage.py test`                     | `php artisan test`                   | `mix test`                           |
| CLI         | `autumn destroy <generator>` (revert a generator)                                   | `rails destroy` (exact namesake)     | —                                    | —                                    | —                                    |
| CLI         | `autumn i18n check [--strict]` (missing/unused keys)                                 | `i18n-tasks health`                  | `makemessages` / lint                | lang linters                         | `gettext.extract --check-up-to-date` |
| Ops         | actuator live logger reload + `/info` provenance (`LogLevels::set_logger_level`, `BuildProvenance`) | —                     | —                                    | Telescope / Horizon                  | —                                    |

## Concept Translation Cheat Sheet

| Concept                | Spring Boot            | Django                | Rails                  | Autumn                          |
|------------------------|------------------------|-----------------------|------------------------|---------------------------------|
| Entry point            | `@SpringBootApplication` | `manage.py`         | `config/application.rb` | `#[autumn_web::main]`          |
| Route definition       | `@GetMapping`          | `urlpatterns`         | `routes.rb`            | `#[get("/path")]`              |
| Request handler        | Controller method      | View function         | Controller action      | `async fn` with extractors      |
| DI container           | Spring IoC             | N/A (manual)          | N/A (manual)           | Axum extractors                 |
| ORM                    | JPA/Hibernate          | Django ORM            | Active Record          | Diesel                          |
| Data model             | `@Entity`              | `models.Model`        | `ActiveRecord::Base`   | `#[model]`                      |
| Repository             | `JpaRepository`        | Manager               | Active Record          | `#[repository(Model)]`          |
| Service layer          | `@Service`             | Service class         | Service object         | `#[service]`                    |
| Validation             | `@Valid`               | `Form.is_valid()`     | `validates`            | `Valid<T>` + `validator`        |
| Error handling         | `@ExceptionHandler`    | Middleware             | `rescue_from`          | `AutumnResult` + `?`            |
| Auth annotation        | `@PreAuthorize`        | `@login_required`     | `before_action`        | `#[secured("role")]`            |
| Config file            | `application.yml`      | `settings.py`         | `config/*.yml`         | `autumn.toml`                   |
| Profiles               | `spring.profiles`      | `DJANGO_SETTINGS`     | `RAILS_ENV`            | `AUTUMN_PROFILE`                |
| Background tasks       | `@Scheduled`           | Celery                | Sidekiq                | `#[scheduled(every = "5m")]`    |
| Durable workflows      | Spring Batch / Camunda | Celery canvas         | Sidekiq batches        | Autumn Harvest (`autumn-harvest`) |
| Template engine        | Thymeleaf              | Django templates      | ERB                    | Maud (compile-time HTML)        |
| Middleware             | Servlet Filter         | Middleware             | Rack middleware         | Tower layers                    |
| Health check           | Actuator               | Custom                | Custom                 | Built-in `/health`              |
| Migrations             | Flyway/Liquibase       | `manage.py migrate`   | `rails db:migrate`     | `diesel migration run`          |
| CLI                    | Spring CLI             | `manage.py`           | `rails`                | `autumn`                        |
| Hot reload             | Spring DevTools        | Auto-reload           | `rails s`              | `autumn dev`                    |
| Request inspector / N+1 detection | `rack-mini-profiler` + `bullet` | Django Debug Toolbar | `rack-mini-profiler` + `bullet` | `/_autumn/inspect` (dev only) |
| Batched iteration      | N/A (JPA scroll)       | `.iterator()`         | `find_each` / `find_in_batches` | `repo.find_each(500)` / `repo.find_in_batches(1000)` |
| Find-or-create         | `getOrSave`            | `get_or_create()`     | `find_or_create_by`    | `repo.find_or_create_by_<field>()` |
| Env file loading       | `.env` (spring-dotenv) | `django-environ`      | `dotenv-rails`         | `.env` auto-load (dev/test)     |
| Response cache headers | `@Cacheable` / `Cache-Control` | `cache_control` decorator | `expires_in` / `fresh_when` | `cache_for(Duration)` → `CacheControl` |
| Model-bound form       | Thymeleaf `th:object`  | `ModelForm`           | `form_for` / `form_with` | `form_for(&changeset, action, method)` |

---

## The Mindset Shift

### No runtime reflection

Spring, Django, and Rails all use runtime introspection to discover
controllers, models, and services. Autumn resolves everything at compile time.
If it compiles, the wiring is correct.

### Errors are values, not exceptions

There is no `try/catch`. Errors flow through `Result<T, E>` and the `?`
operator. This means every error path is visible in the type signature.

### No global state

Spring has an application context. Django has `settings`. Rails has
`Rails.application`. Autumn passes state explicitly through extractors. If a
handler needs the database, it declares `db: Db` in its parameters.

### Compile-time guarantees

- Type-safe SQL queries (Diesel catches column mismatches at compile time)
- Type-safe HTML templates (Maud is Rust code, not string interpolation)
- Type-safe route parameters (a `Path<i32>` that receives "abc" fails at
  the extractor, not in your handler)

The compiler catches more, so the runtime surprises you less.
