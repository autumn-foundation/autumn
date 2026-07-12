Prompt-Version: v1

# Build brief: Habit Tracker — Spring Boot

Read `prompts/_core.md` and `SPEC.md` first; they define the app, the exact JSON
API contract, and the streak semantics. Everything there applies. Below are the
Spring Boot-specific bootstrapping notes.

## Framework facts

- Use **Spring Boot** (Java or Kotlin) with **Spring Web**, **Spring Data JPA**,
  and a template engine (**Thymeleaf**) for the HTML view.
- Scaffold via Spring Initializr (`spring init` CLI or start.spring.io)
  with dependencies: `web`, `data-jpa`, `thymeleaf`, plus a database driver
  (H2 in-memory or Postgres — H2 file/mem is fine for the benchmark).
- **Entities.**
  - `Habit` (`id`, `name`, `description` nullable, `createdAt`).
  - `Completion` (`id`, `@ManyToOne Habit`, `LocalDate date`) with a unique
    constraint on `(habit_id, date)`
    (`@Table(uniqueConstraints=@UniqueConstraint(columnNames={"habit_id","date"}))`).
    A `DataIntegrityViolationException` on insert → **409**. Cascade delete of
    completions when a habit is removed (`cascade = ALL, orphanRemoval = true`).
- **Controllers.**
  - `@RestController @RequestMapping("/api/habits")` with `@PostMapping`,
    `@GetMapping`, `@GetMapping("/{id}")`, `@PutMapping("/{id}")`,
    `@DeleteMapping("/{id}")`, `@PostMapping("/{id}/complete")`. Return
    `ResponseEntity` with explicit statuses (`201`, `200`, `204`, `409`, `404`,
    `422`/`400`).
  - A separate `@Controller` with `@GetMapping("/")` returning a Thymeleaf
    template name → `text/html` listing habits.
- **DTOs.** Response DTOs matching `SPEC.md` field names (`created_at`,
  `current_streak`, `history`). Configure Jackson snake_case for these fields
  (e.g. `@JsonProperty("created_at")` or a `PropertyNamingStrategy`), since the
  contract uses snake_case.
- **Validation.** `@NotBlank` on `name` (via `@Valid`) → 422/400 (a
  `@ControllerAdvice` mapping `MethodArgumentNotValidException` to 422). Parse
  the date with `LocalDate.parse(...)`; catch `DateTimeParseException` → 422/400.
- **Streak.** Compute `current_streak` per `SPEC.md` §3 from the completion
  dates; `history` = dates sorted descending as ISO `YYYY-MM-DD` strings.
- **Seed / demo data.** A `CommandLineRunner` bean (or `data.sql`) inserting a
  couple of demo habits with completions on startup.
- **Tests.** JUnit + `@SpringBootTest` / `MockMvc` or `TestRestTemplate`.

## run.sh

```sh
#!/usr/bin/env sh
set -e
./mvnw -q package -DskipTests    # or ./gradlew bootJar
exec java -jar target/*.jar --server.port="${PORT:-8080}"
```

(Or `./mvnw spring-boot:run -Dspring-boot.run.arguments=--server.port=${PORT:-8080}`.)
The Java process blocks while the server runs.
