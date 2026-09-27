# Default locale for the blog example.
#
# Keys follow a `<page>.<element>` convention. Argument substitutions use
# Project Fluent's `{ $name }` placeable syntax.

# ── Layout ────────────────────────────────────────────────────
layout.skip_to_content = Skip to main content
nav.brand = Autumn Blog
nav.home = Home
nav.about = About
nav.greet = Greet
nav.admin = Admin
nav.new_post = New Post
nav.locale.label = Language

footer.tagline = Built with Autumn

# ── Home page ─────────────────────────────────────────────────
home.hero.title = Welcome to the Blog
home.hero.subtitle = Thoughts, tutorials, and stories — powered by Autumn.

# ── Greet page (demonstrates t! macro) ───────────────────────
greet.title = Welcome to the Autumn Blog
greet.greeting = Hello, { $name }! You are viewing this page in English.
greet.switcher_help = Switch language using the links below — each one is a real, crawlable /{locale}/greet URL.
greet.try = Try visiting /es/greet directly, or /greet to be redirected to your negotiated locale.
