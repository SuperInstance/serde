# serde guardian — conservation-aware serialization

You know that feeling when you serialize a struct and the JSON payload is 4MB, and it turns out 3.8MB is one base64-encoded avatar image?

Yeah. This fixes that.

## What it does

`guardian` is a feature-gated module for serde that wraps any `Serializer` and:

1. **Enforces budgets** — set max bytes, max fields, max nesting depth. Get an error when you blow past them.
2. **Tracks field costs** — which fields are eating your bytes, which structs are the worst offenders.
3. **Profiles serialization** — nesting distribution, type frequency, serialization call counts.
4. **Generates conservation reports** — human-readable output that tells you *exactly* where to trim.

## Enable it

```toml
[dependencies]
serde = { version = "1.0", features = ["guardian"] }
```

## Use it

```rust
use serde::guardian::{SerializationBudget, BudgetGuard, SerializationProfile};
use serde::Serialize;

#[derive(Serialize)]
struct User {
    name: String,
    email: String,
    avatar_url: String,  // ← 4MB base64, every request
    bio: String,
}

let budget = SerializationBudget::new()
    .max_bytes(4096)
    .max_fields(100)
    .max_nesting_depth(5);

let profile = SerializationProfile::new();

// Wrap any serializer
let mut output = Vec::new();
let json_ser = serde_json::Serializer::new(&mut output);
let guard = BudgetGuard::new(json_ser, budget, profile);

match user.serialize(guard) {
    Ok(_) => println!("Serialized within budget"),
    Err(e) => println!("Budget exceeded: {}", e),
}

// Get the profile back and generate a report
let profile = guard.finish();
println!("{}", profile.conservation_report("User"));
```

## The report

```
═══ Conservation Report: User ═══

  Serialization calls: 1
  Max nesting depth:   1
  Total fields:        4
  Estimated bytes:     4003543

  Top fields by byte cost:
                             User.avatar_url            4000000 bytes (99.9%)
                             User.bio                         320 bytes (0.0%)
                             User.name                         12 bytes (0.0%)
                             User.email                        18 bytes (0.0%)

  ⚠ Top 3 fields = 100% of bytes. Consider #[serde(skip_serializing)] on the heaviest.

════════════════════════════════════
```

That's it. That's the whole pitch. One field eating 99.9% of your bytes.

## The types

### `SerializationBudget`

```rust
let budget = SerializationBudget::new()
    .max_bytes(4096)        // error if serialized output exceeds 4KB
    .max_fields(200)        // error if more than 200 fields total
    .max_nesting_depth(5);  // error if structs nest deeper than 5 levels
```

All limits are optional. `SerializationBudget::new()` gives you an unlimited budget (useful for profiling without enforcement).

### `BudgetGuard<S>`

Wraps any `Serializer`. Enforces the budget. Returns `GuardianError` on overflow.

```rust
let guard = BudgetGuard::new(my_serializer, budget, profile);
data.serialize(guard)?;          // returns Err(GuardianError) if budget exceeded
let profile = guard.finish();    // get the recorded profile
```

### `FieldCounter`

Tracks field counts and byte costs per field and per struct. Usually used through `SerializationProfile`, but you can use it standalone:

```rust
let mut counter = FieldCounter::new();
counter.record_field("User", "name", 12);
counter.record_field("User", "avatar_url", 4000000);
let top = counter.top_fields(3);  // Vec<(name, bytes, percentage)>
```

### `SerializationProfile`

Records everything: field costs, nesting distribution, type frequency, call counts.

```rust
let mut profile = SerializationProfile::new();
profile.record_field("User", "name", 12);
profile.record_type("User");
profile.enter_nested();
println!("{}", profile.conservation_report("User"));
```

### `GuardianError`

```rust
enum GuardianError {
    ByteLimitExceeded { current: usize, limit: usize },
    FieldLimitExceeded { current: usize, limit: usize },
    NestingLimitExceeded { current: usize, limit: usize },
    Custom(String),
}
```

Implements `serde::ser::Error` and `std::error::Error`.

## When to use this

- **API responses** — stop accidentally sending megabytes when kilobytes would do
- **WebSocket messages** — budget enforcement before you hit the wire
- **Embedded systems** — serialize within memory constraints
- **Development** — profile your serialization hotspots during testing
- **CI** — fail tests if serialization budgets are exceeded

## When not to use this

- Hot paths in production where every nanosecond counts (the tracking has overhead)
- `no_std` environments (requires `std` for `BTreeMap` and `std::error::Error`)

## License

Same as serde: MIT OR Apache-2.0.
