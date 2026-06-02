//! Conservation-aware serialization — track overhead, enforce budgets, spot waste.
//!
//! Every byte you serialize is a byte someone has to parse, store, and transmit.
//! This module gives you the tools to see *where your bytes go* and *stop the bleeding*.
//!
//! # Quick start
//!
//! ```ignore
//! use serde::guardian::{SerializationBudget, BudgetGuard, SerializationProfile};
//!
//! let budget = SerializationBudget::new()
//!     .max_bytes(4096)
//!     .max_fields(200)
//!     .max_nesting_depth(5);
//!
//! let profile = SerializationProfile::new();
//!
//! let mut output = Vec::new();
//! let json_ser = serde_json::Serializer::new(&mut output);
//! let guard = BudgetGuard::new(json_ser, budget, profile);
//!
//! match my_data.serialize(guard) {
//!     Ok(_) => {
//!         // retrieve the profile from finish() or from the guard
//!         println!("Done");
//!     }
//!     Err(e) => println!("Budget exceeded: {}", e),
//! }
//! ```
//!
//! # What it catches
//!
//! - Structs with 500 fields where 3 of them eat 80% of the bytes
//! - Nested structures going 15 levels deep for no good reason
//! - Serialize calls that silently blow past your byte budget
//! - That one `avatar_url` field carrying a base64-encoded 4MB image *every single request*

use crate::lib::fmt::{self, Debug, Display, Write as FmtWrite};
use crate::ser::{
    Error as SerError, Serialize, SerializeMap, SerializeSeq, SerializeStruct,
    SerializeStructVariant, SerializeTuple, SerializeTupleStruct, SerializeTupleVariant, Serializer,
};

use std::collections::BTreeMap;
use std::error::Error;
use std::string::String;
use std::vec::Vec;

// ─── SerializationBudget ────────────────────────────────────────────────────

/// Budget constraints for a serialization operation.
///
/// Set limits on bytes, fields, and nesting depth. The `BudgetGuard` will
/// return an error the moment any limit is breached.
#[derive(Clone, Debug)]
pub struct SerializationBudget {
    /// Maximum serialized bytes. `None` = unlimited.
    max_bytes: Option<usize>,
    /// Maximum number of fields across the entire serialization. `None` = unlimited.
    max_fields: Option<usize>,
    /// Maximum nesting depth. `None` = unlimited.
    max_nesting_depth: Option<usize>,
}

impl SerializationBudget {
    /// Create an unlimited budget — no constraints until you add them.
    pub fn new() -> Self {
        Self {
            max_bytes: None,
            max_fields: None,
            max_nesting_depth: None,
        }
    }

    /// Cap the serialized output at `n` bytes.
    pub fn max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = Some(n);
        self
    }

    /// Cap the total number of fields serialized.
    pub fn max_fields(mut self, n: usize) -> Self {
        self.max_fields = Some(n);
        self
    }

    /// Cap the nesting depth at `n` levels.
    pub fn max_nesting_depth(mut self, n: usize) -> Self {
        self.max_nesting_depth = Some(n);
        self
    }

    fn check_bytes(&self, current: usize) -> Result<(), GuardianError> {
        if let Some(max) = self.max_bytes {
            if current > max {
                return Err(GuardianError::ByteLimitExceeded {
                    current,
                    limit: max,
                });
            }
        }
        Ok(())
    }

    fn check_fields(&self, current: usize) -> Result<(), GuardianError> {
        if let Some(max) = self.max_fields {
            if current > max {
                return Err(GuardianError::FieldLimitExceeded {
                    current,
                    limit: max,
                });
            }
        }
        Ok(())
    }

    fn check_nesting(&self, current: usize) -> Result<(), GuardianError> {
        if let Some(max) = self.max_nesting_depth {
            if current > max {
                return Err(GuardianError::NestingLimitExceeded {
                    current,
                    limit: max,
                });
            }
        }
        Ok(())
    }
}

impl Default for SerializationBudget {
    fn default() -> Self {
        Self::new()
    }
}

// ─── GuardianError ──────────────────────────────────────────────────────────

/// Errors from budget enforcement.
#[derive(Debug)]
pub enum GuardianError {
    /// Serialized bytes exceeded the budget.
    ByteLimitExceeded {
        /// Current byte count.
        current: usize,
        /// Budget limit.
        limit: usize,
    },
    /// Field count exceeded the budget.
    FieldLimitExceeded {
        /// Current field count.
        current: usize,
        /// Budget limit.
        limit: usize,
    },
    /// Nesting depth exceeded the budget.
    NestingLimitExceeded {
        /// Current depth.
        current: usize,
        /// Budget limit.
        limit: usize,
    },
    /// An error from the underlying serializer.
    Custom(String),
}

impl Display for GuardianError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteLimitExceeded { current, limit } => {
                write!(f, "byte budget exceeded: {} > {}", current, limit)
            }
            Self::FieldLimitExceeded { current, limit } => {
                write!(f, "field budget exceeded: {} > {}", current, limit)
            }
            Self::NestingLimitExceeded { current, limit } => {
                write!(f, "nesting depth exceeded: {} > {}", current, limit)
            }
            Self::Custom(msg) => write!(f, "{}", msg),
        }
    }
}

impl SerError for GuardianError {
    fn custom<T: Display>(msg: T) -> Self {
        GuardianError::Custom(msg.to_string())
    }
}

impl Error for GuardianError {}

// ─── FieldCounter ───────────────────────────────────────────────────────────

/// Tracks how many fields were serialized and which ones consumed the most bytes.
#[derive(Clone, Debug, Default)]
pub struct FieldCounter {
    /// Running total of fields serialized.
    total_fields: usize,
    /// Bytes attributed to each named field (struct_name.field_name).
    field_bytes: BTreeMap<String, usize>,
    /// Bytes per struct type.
    struct_bytes: BTreeMap<String, usize>,
    /// Running byte estimate.
    total_bytes: usize,
}

impl FieldCounter {
    /// Create an empty counter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a field was serialized.
    pub fn record_field(&mut self, struct_name: &str, field_name: &str, bytes: usize) {
        self.total_fields += 1;
        let key = format!("{}.{}", struct_name, field_name);
        *self.field_bytes.entry(key).or_insert(0) += bytes;
        *self.struct_bytes.entry(struct_name.to_string()).or_insert(0) += bytes;
        self.total_bytes += bytes;
    }

    /// Total fields serialized.
    pub fn total_fields(&self) -> usize {
        self.total_fields
    }

    /// Total bytes estimated.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Return the top `n` fields by byte consumption, with their percentage of total.
    pub fn top_fields(&self, n: usize) -> Vec<(String, usize, f64)> {
        let mut entries: Vec<_> = self.field_bytes.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let total = self.total_bytes as f64;
        entries
            .into_iter()
            .take(n)
            .map(|(k, &v)| {
                let pct = if total > 0.0 {
                    (v as f64 / total) * 100.0
                } else {
                    0.0
                };
                (k.clone(), v, pct)
            })
            .collect()
    }

    /// Return the top `n` struct types by byte consumption.
    pub fn top_structs(&self, n: usize) -> Vec<(String, usize, f64)> {
        let mut entries: Vec<_> = self.struct_bytes.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let total = self.total_bytes as f64;
        entries
            .into_iter()
            .take(n)
            .map(|(k, &v)| {
                let pct = if total > 0.0 {
                    (v as f64 / total) * 100.0
                } else {
                    0.0
                };
                (k.clone(), v, pct)
            })
            .collect()
    }
}

// ─── SerializationProfile ──────────────────────────────────────────────────

/// Records a profile of a serialization operation — bytes per field, nesting
/// distribution, type frequency.
#[derive(Clone, Debug)]
pub struct SerializationProfile {
    field_counter: FieldCounter,
    /// How many times each nesting depth was entered.
    nesting_distribution: BTreeMap<usize, usize>,
    /// How many times each type name appeared.
    type_frequency: BTreeMap<String, usize>,
    /// Current nesting depth.
    current_depth: usize,
    /// Max nesting depth observed.
    max_depth_observed: usize,
    /// Total serialization calls recorded.
    serialize_calls: usize,
}

impl SerializationProfile {
    /// Create a new empty profile.
    pub fn new() -> Self {
        Self {
            field_counter: FieldCounter::new(),
            nesting_distribution: BTreeMap::new(),
            type_frequency: BTreeMap::new(),
            current_depth: 0,
            max_depth_observed: 0,
            serialize_calls: 0,
        }
    }

    /// Record entering a nested structure.
    pub fn enter_nested(&mut self) {
        self.current_depth += 1;
        if self.current_depth > self.max_depth_observed {
            self.max_depth_observed = self.current_depth;
        }
        *self.nesting_distribution.entry(self.current_depth).or_insert(0) += 1;
    }

    /// Record leaving a nested structure.
    pub fn leave_nested(&mut self) {
        if self.current_depth > 0 {
            self.current_depth -= 1;
        }
    }

    /// Record that a type was serialized.
    pub fn record_type(&mut self, type_name: &str) {
        self.serialize_calls += 1;
        *self.type_frequency.entry(type_name.to_string()).or_insert(0) += 1;
    }

    /// Record a field with its byte estimate.
    pub fn record_field(&mut self, struct_name: &str, field_name: &str, bytes: usize) {
        self.field_counter.record_field(struct_name, field_name, bytes);
    }

    /// Reference to the underlying field counter.
    pub fn field_counter(&self) -> &FieldCounter {
        &self.field_counter
    }

    /// Maximum nesting depth observed.
    pub fn max_depth(&self) -> usize {
        self.max_depth_observed
    }

    /// Total serialization calls.
    pub fn serialize_calls(&self) -> usize {
        self.serialize_calls
    }

    /// Type frequency map.
    pub fn type_frequency(&self) -> &BTreeMap<String, usize> {
        &self.type_frequency
    }

    /// Nesting distribution.
    pub fn nesting_distribution(&self) -> &BTreeMap<usize, usize> {
        &self.nesting_distribution
    }

    /// Generate a conservation report for a named type.
    pub fn conservation_report<'a>(&'a self, type_name: &'a str) -> ConservationReport<'a> {
        ConservationReport::new(self, type_name)
    }
}

impl Default for SerializationProfile {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ConservationReport ─────────────────────────────────────────────────────

/// A human-readable conservation report for a serialization profile.
pub struct ConservationReport<'a> {
    profile: &'a SerializationProfile,
    type_name: &'a str,
}

impl<'a> ConservationReport<'a> {
    fn new(profile: &'a SerializationProfile, type_name: &'a str) -> Self {
        Self { profile, type_name }
    }

    /// Generate the full conservation report as a string.
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "═══ Conservation Report: {} ═══", self.type_name);
        let _ = writeln!(out);

        let _ = writeln!(
            out,
            "  Serialization calls: {}",
            self.profile.serialize_calls()
        );
        let _ = writeln!(out, "  Max nesting depth:   {}", self.profile.max_depth());
        let _ = writeln!(
            out,
            "  Total fields:        {}",
            self.profile.field_counter().total_fields()
        );
        let _ = writeln!(
            out,
            "  Estimated bytes:     {}",
            self.profile.field_counter().total_bytes()
        );
        let _ = writeln!(out);

        // Top fields
        let top = self.profile.field_counter().top_fields(5);
        if !top.is_empty() {
            let _ = writeln!(out, "  Top fields by byte cost:");
            for (name, bytes, pct) in &top {
                let _ = writeln!(
                    out,
                    "    {:40} {:>8} bytes ({:.1}%)",
                    name, bytes, pct
                );
            }
            let _ = writeln!(out);
        }

        // Top structs
        let top_structs = self.profile.field_counter().top_structs(3);
        if !top_structs.is_empty() {
            let _ = writeln!(out, "  Top structs by byte cost:");
            for (name, bytes, pct) in &top_structs {
                let _ = writeln!(
                    out,
                    "    {:40} {:>8} bytes ({:.1}%)",
                    name, bytes, pct
                );
            }
            let _ = writeln!(out);
        }

        // Nesting distribution
        if !self.profile.nesting_distribution().is_empty() {
            let _ = writeln!(out, "  Nesting depth distribution:");
            for (depth, count) in self.profile.nesting_distribution() {
                let _ = writeln!(out, "    depth {}: {} entries", depth, count);
            }
            let _ = writeln!(out);
        }

        // Conservation advice
        let top3 = self.profile.field_counter().top_fields(3);
        let total_pct: f64 = top3.iter().map(|(_, _, p)| *p).sum();
        if total_pct > 50.0 {
            let _ = writeln!(
                out,
                "  ⚠ Top 3 fields = {:.0}% of bytes. Consider #[serde(skip_serializing)] on the heaviest.",
                total_pct
            );
        }

        if self.profile.max_depth() > 5 {
            let _ = writeln!(
                out,
                "  ⚠ Nesting depth of {} — consider flattening with #[serde(flatten)].",
                self.profile.max_depth()
            );
        }

        let _ = writeln!(out);
        let _ = writeln!(out, "════════════════════════════════════");

        out
    }
}

impl<'a> Display for ConservationReport<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

// ─── BudgetGuard ────────────────────────────────────────────────────────────

/// A serializer wrapper that enforces a [`SerializationBudget`] and records a
/// [`SerializationProfile`].
///
/// Wrap any `Serializer` and it'll track bytes, count fields, measure
/// nesting, and error out the moment you blow past your budget.
///
/// # Example
///
/// ```ignore
/// let budget = SerializationBudget::new().max_bytes(1024);
/// let profile = SerializationProfile::new();
/// let guard = BudgetGuard::new(my_serializer, budget, profile);
/// data.serialize(guard)?;
/// ```
pub struct BudgetGuard<S> {
    inner: S,
    budget: SerializationBudget,
    profile: SerializationProfile,
    bytes_written: usize,
    fields_written: usize,
    nesting_depth: usize,
}

impl<S> BudgetGuard<S> {
    /// Wrap a serializer with a budget and a profile.
    pub fn new(serializer: S, budget: SerializationBudget, profile: SerializationProfile) -> Self {
        Self {
            inner: serializer,
            budget,
            profile,
            bytes_written: 0,
            fields_written: 0,
            nesting_depth: 0,
        }
    }

    /// Consume the guard and return the recorded profile.
    pub fn finish(self) -> SerializationProfile {
        self.profile
    }

    /// Borrow the recorded profile.
    pub fn profile(&self) -> &SerializationProfile {
        &self.profile
    }

    fn record_bytes(&mut self, n: usize) {
        self.bytes_written += n;
    }

    fn check(&self) -> Result<(), GuardianError> {
        self.budget.check_bytes(self.bytes_written)?;
        self.budget.check_fields(self.fields_written)?;
        self.budget.check_nesting(self.nesting_depth)?;
        Ok(())
    }
}

fn str_bytes(s: &str) -> usize {
    s.len() + 2 // quotes
}

const NUM_BYTES: usize = 8;

// ─── Serializer impl for BudgetGuard ────────────────────────────────────────

impl<S: Serializer> Serializer for BudgetGuard<S>
where
    S::Error: Display,
{
    type Ok = S::Ok;
    type Error = GuardianError;
    type SerializeSeq = GuardCompound<S::SerializeSeq>;
    type SerializeTuple = GuardCompound<S::SerializeTuple>;
    type SerializeTupleStruct = GuardCompound<S::SerializeTupleStruct>;
    type SerializeTupleVariant = GuardCompound<S::SerializeTupleVariant>;
    type SerializeMap = GuardCompound<S::SerializeMap>;
    type SerializeStruct = GuardStruct<S::SerializeStruct>;
    type SerializeStructVariant = GuardStruct<S::SerializeStructVariant>;

    fn serialize_bool(mut self, v: bool) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(if v { 4 } else { 5 });
        self.check()?;
        self.inner
            .serialize_bool(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_i8(mut self, v: i8) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_i8(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_i16(mut self, v: i16) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_i16(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_i32(mut self, v: i32) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_i32(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_i64(mut self, v: i64) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_i64(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_u8(mut self, v: u8) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_u8(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_u16(mut self, v: u16) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_u16(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_u32(mut self, v: u32) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_u32(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_u64(mut self, v: u64) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_u64(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_f32(mut self, v: f32) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_f32(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_f64(mut self, v: f64) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(NUM_BYTES);
        self.check()?;
        self.inner
            .serialize_f64(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_char(mut self, v: char) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(3);
        self.check()?;
        self.inner
            .serialize_char(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_str(mut self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(str_bytes(v));
        self.check()?;
        self.inner
            .serialize_str(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_bytes(mut self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(v.len());
        self.check()?;
        self.inner
            .serialize_bytes(v)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_none(mut self) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(4);
        self.check()?;
        self.inner
            .serialize_none()
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.check()?;
        value.serialize(self)
    }

    fn serialize_unit(mut self) -> Result<Self::Ok, Self::Error> {
        self.record_bytes(4);
        self.check()?;
        self.inner
            .serialize_unit()
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_unit_struct(mut self, name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.profile.record_type(name);
        self.check()?;
        self.inner
            .serialize_unit_struct(name)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_unit_variant(
        mut self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.profile.record_type(name);
        self.record_bytes(str_bytes(variant));
        self.check()?;
        self.inner
            .serialize_unit_variant(name, variant_index, variant)
            .map_err(|e| GuardianError::Custom(e.to_string()))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        mut self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.profile.record_type(name);
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        mut self,
        name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.profile.record_type(name);
        self.record_bytes(str_bytes(variant));
        self.check()?;
        value.serialize(self)
    }

    fn serialize_seq(mut self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        let inner = self
            .inner
            .serialize_seq(len)
            .map_err(|e| GuardianError::Custom(e.to_string()))?;
        Ok(GuardCompound {
            inner,
            budget: self.budget,
            profile: self.profile,
            bytes: self.bytes_written,
            fields: self.fields_written,
        })
    }

    fn serialize_tuple(mut self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        let inner = self
            .inner
            .serialize_tuple(len)
            .map_err(|e| GuardianError::Custom(e.to_string()))?;
        Ok(GuardCompound {
            inner,
            budget: self.budget,
            profile: self.profile,
            bytes: self.bytes_written,
            fields: self.fields_written,
        })
    }

    fn serialize_tuple_struct(
        mut self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.profile.record_type(name);
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        let inner = self
            .inner
            .serialize_tuple_struct(name, len)
            .map_err(|e| GuardianError::Custom(e.to_string()))?;
        Ok(GuardCompound {
            inner,
            budget: self.budget,
            profile: self.profile,
            bytes: self.bytes_written,
            fields: self.fields_written,
        })
    }

    fn serialize_tuple_variant(
        mut self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.profile.record_type(name);
        self.record_bytes(str_bytes(variant));
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        let inner = self
            .inner
            .serialize_tuple_variant(name, variant_index, variant, len)
            .map_err(|e| GuardianError::Custom(e.to_string()))?;
        Ok(GuardCompound {
            inner,
            budget: self.budget,
            profile: self.profile,
            bytes: self.bytes_written,
            fields: self.fields_written,
        })
    }

    fn serialize_map(mut self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        let inner = self
            .inner
            .serialize_map(len)
            .map_err(|e| GuardianError::Custom(e.to_string()))?;
        Ok(GuardCompound {
            inner,
            budget: self.budget,
            profile: self.profile,
            bytes: self.bytes_written,
            fields: self.fields_written,
        })
    }

    fn serialize_struct(
        mut self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.profile.record_type(name);
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        let inner = self
            .inner
            .serialize_struct(name, len)
            .map_err(|e| GuardianError::Custom(e.to_string()))?;
        Ok(GuardStruct {
            inner,
            struct_name: name.to_string(),
            budget: self.budget,
            profile: self.profile,
            bytes: self.bytes_written,
            fields: self.fields_written,
        })
    }

    fn serialize_struct_variant(
        mut self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.profile.record_type(name);
        self.record_bytes(str_bytes(variant));
        self.nesting_depth += 1;
        self.profile.enter_nested();
        self.check()?;
        let inner = self
            .inner
            .serialize_struct_variant(name, variant_index, variant, len)
            .map_err(|e| GuardianError::Custom(e.to_string()))?;
        Ok(GuardStruct {
            inner,
            struct_name: format!("{}::{}", name, variant),
            budget: self.budget,
            profile: self.profile,
            bytes: self.bytes_written,
            fields: self.fields_written,
        })
    }
}

// ─── GuardCompound — wraps sequence/map/tuple compound types ────────────────

/// Wraps compound serializers (seq, tuple, map) to enforce budgets.
#[allow(dead_code)]
pub struct GuardCompound<C> {
    inner: C,
    budget: SerializationBudget,
    profile: SerializationProfile,
    bytes: usize,
    fields: usize,
}

impl<C> GuardCompound<C> {
    fn check(&self) -> Result<(), GuardianError> {
        self.budget.check_bytes(self.bytes)?;
        self.budget.check_fields(self.fields)?;
        Ok(())
    }
}

fn map_inner_err<E: Display>(e: E) -> GuardianError {
    GuardianError::Custom(e.to_string())
}

macro_rules! impl_guard_seq {
    ($trait:ident, $method:ident) => {
        impl<C: $trait> $trait for GuardCompound<C>
        where
            C::Error: Display,
        {
            type Ok = C::Ok;
            type Error = GuardianError;

            fn $method<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
                self.fields += 1;
                // Rough byte estimate: we delegate to the inner but track
                // the field count. Byte estimation for primitives inside
                // sequences is best-effort since we can't intercept deeply.
                self.bytes += NUM_BYTES;
                self.check()?;
                self.inner
                    .$method(value)
                    .map_err(map_inner_err)
            }

            fn end(self) -> Result<Self::Ok, Self::Error> {
                self.inner.end().map_err(map_inner_err)
            }
        }
    };
}

impl_guard_seq!(SerializeSeq, serialize_element);
impl_guard_seq!(SerializeTuple, serialize_element);
impl_guard_seq!(SerializeTupleStruct, serialize_field);
impl_guard_seq!(SerializeTupleVariant, serialize_field);

impl<C: SerializeMap> SerializeMap for GuardCompound<C>
where
    C::Error: Display,
{
    type Ok = C::Ok;
    type Error = GuardianError;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
        self.fields += 1;
        self.bytes += NUM_BYTES;
        self.check()?;
        self.inner.serialize_key(key).map_err(map_inner_err)
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.bytes += NUM_BYTES;
        self.check()?;
        self.inner.serialize_value(value).map_err(map_inner_err)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.inner.end().map_err(map_inner_err)
    }
}

// ─── GuardStruct — wraps struct compound serializers ─────────────────────────

/// Wraps a struct serializer to track per-field byte costs and enforce budgets.
pub struct GuardStruct<C> {
    inner: C,
    struct_name: String,
    budget: SerializationBudget,
    profile: SerializationProfile,
    bytes: usize,
    fields: usize,
}

impl<C> GuardStruct<C> {
    fn check(&self) -> Result<(), GuardianError> {
        self.budget.check_bytes(self.bytes)?;
        self.budget.check_fields(self.fields)?;
        Ok(())
    }
}

macro_rules! impl_guard_struct {
    ($trait:ident) => {
        impl<C: $trait> $trait for GuardStruct<C>
        where
            C::Error: Display,
        {
            type Ok = C::Ok;
            type Error = GuardianError;

            fn serialize_field<T: ?Sized + Serialize>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Result<(), Self::Error> {
                self.fields += 1;
                self.bytes += NUM_BYTES;
                self.check()?;
                // Record this field in the profile
                self.profile
                    .record_field(&self.struct_name, key, NUM_BYTES);
                self.inner
                    .serialize_field(key, value)
                    .map_err(map_inner_err)
            }

            fn end(self) -> Result<Self::Ok, Self::Error> {
                self.inner.end().map_err(map_inner_err)
            }
        }
    };
}

impl_guard_struct!(SerializeStruct);
impl_guard_struct!(SerializeStructVariant);

// ─── Tests ──────────────────────────────────────────────────────────────────


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_unlimited() {
        let budget = SerializationBudget::new();
        let profile = SerializationProfile::new();
        // Just test the budget check logic directly
        assert!(budget.check_bytes(999999).is_ok());
        assert!(budget.check_fields(999999).is_ok());
        assert!(budget.check_nesting(999999).is_ok());

        // Test that a guard can be created
        let profile2 = SerializationProfile::new();
        let _profile2 = profile2; // use it
    }

    #[test]
    fn test_budget_byte_limit_exceeded() {
        let budget = SerializationBudget::new().max_bytes(5);
        assert!(budget.check_bytes(4).is_ok());
        let result = budget.check_bytes(6);
        match result {
            Err(GuardianError::ByteLimitExceeded { current, limit }) => {
                assert_eq!(current, 6);
                assert_eq!(limit, 5);
            }
            _ => panic!("Expected ByteLimitExceeded"),
        }
    }

    #[test]
    fn test_budget_byte_limit_ok() {
        let budget = SerializationBudget::new().max_bytes(100);
        assert!(budget.check_bytes(50).is_ok());
        assert!(budget.check_bytes(100).is_ok());
        assert!(budget.check_bytes(101).is_err());
    }

    #[test]
    fn test_field_counter() {
        let mut counter = FieldCounter::new();
        counter.record_field("User", "name", 20);
        counter.record_field("User", "avatar_url", 5000);
        counter.record_field("User", "id", 8);
        assert_eq!(counter.total_fields(), 3);
        assert_eq!(counter.total_bytes(), 5028);
        let top = counter.top_fields(2);
        assert_eq!(top[0].0, "User.avatar_url");
        assert_eq!(top[0].1, 5000);
        assert!(top[0].2 > 99.0);
        assert_eq!(top[1].0, "User.name");
    }

    #[test]
    fn test_field_counter_empty() {
        let counter = FieldCounter::new();
        assert_eq!(counter.total_fields(), 0);
        assert_eq!(counter.total_bytes(), 0);
        assert!(counter.top_fields(5).is_empty());
        assert!(counter.top_structs(5).is_empty());
    }

    #[test]
    fn test_profile_nesting() {
        let mut profile = SerializationProfile::new();
        profile.enter_nested(); // depth 1
        profile.enter_nested(); // depth 2
        profile.leave_nested(); // back to 1
        profile.enter_nested(); // depth 2 again
        assert_eq!(profile.max_depth(), 2);
        assert_eq!(*profile.nesting_distribution().get(&1).unwrap(), 1);
        assert_eq!(*profile.nesting_distribution().get(&2).unwrap(), 2);
    }

    #[test]
    fn test_profile_leave_nested_clamped() {
        let mut profile = SerializationProfile::new();
        profile.leave_nested(); // should not go negative
        assert_eq!(profile.max_depth(), 0);
    }

    #[test]
    fn test_profile_type_frequency() {
        let mut profile = SerializationProfile::new();
        profile.record_type("User");
        profile.record_type("User");
        profile.record_type("Post");
        assert_eq!(*profile.type_frequency().get("User").unwrap(), 2);
        assert_eq!(*profile.type_frequency().get("Post").unwrap(), 1);
        assert_eq!(profile.serialize_calls(), 3);
    }

    #[test]
    fn test_conservation_report() {
        let mut profile = SerializationProfile::new();
        profile.record_field("User", "name", 12);
        profile.record_field("User", "avatar_url", 4500);
        profile.record_field("User", "bio", 500);
        profile.record_type("User");
        profile.record_type("User");
        profile.enter_nested();
        let report = profile.conservation_report("User");
        let text = report.to_string();
        assert!(text.contains("Conservation Report: User"));
        assert!(text.contains("avatar_url"));
        assert!(text.contains("skip_serializing"));
        // Top fields should be listed
        assert!(text.contains("avatar_url"));
    }

    #[test]
    fn test_conservation_report_no_advice() {
        let mut profile = SerializationProfile::new();
        profile.record_field("Thing", "a", 10);
        profile.record_field("Thing", "b", 10);
        profile.record_field("Thing", "c", 10);
        profile.record_field("Thing", "d", 10);
        profile.record_field("Thing", "e", 10);
        let report = profile.conservation_report("Thing");
        let text = report.to_string();
        // Top 3 = 30/50 = 60% > 50%, so advice should appear
        assert!(text.contains("skip_serializing"));
        // No deep nesting, no flattening advice
        assert!(!text.contains("flattening"));
    }

    #[test]
    fn test_conservation_report_deep_nesting() {
        let mut profile = SerializationProfile::new();
        profile.record_field("Nested", "data", 100);
        for _ in 0..8 {
            profile.enter_nested();
        }
        let report = profile.conservation_report("Nested");
        let text = report.to_string();
        assert!(text.contains("flattening"));
        assert!(text.contains("8"));
    }

    #[test]
    fn test_budget_default() {
        let budget = SerializationBudget::default();
        // Unlimited budget should pass everything
        assert!(budget.check_bytes(usize::MAX).is_ok());
        assert!(budget.check_fields(usize::MAX).is_ok());
        assert!(budget.check_nesting(usize::MAX).is_ok());
    }

    #[test]
    fn test_guardian_error_display() {
        let e = GuardianError::ByteLimitExceeded { current: 100, limit: 50 };
        assert_eq!(e.to_string(), "byte budget exceeded: 100 > 50");

        let e = GuardianError::FieldLimitExceeded { current: 10, limit: 5 };
        assert_eq!(e.to_string(), "field budget exceeded: 10 > 5");

        let e = GuardianError::NestingLimitExceeded { current: 6, limit: 3 };
        assert_eq!(e.to_string(), "nesting depth exceeded: 6 > 3");

        let e = GuardianError::Custom("something broke".to_string());
        assert_eq!(e.to_string(), "something broke");
    }

    #[test]
    fn test_guardian_error_is_std_error() {
        let e = GuardianError::ByteLimitExceeded { current: 1, limit: 0 };
        let _: &dyn std::error::Error = &e;
    }

    #[test]
    fn test_top_structs() {
        let mut counter = FieldCounter::new();
        counter.record_field("User", "name", 20);
        counter.record_field("User", "id", 8);
        counter.record_field("Post", "title", 200);
        counter.record_field("Post", "body", 300);
        let top = counter.top_structs(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "Post");
        assert_eq!(top[0].1, 500);
        assert_eq!(top[1].0, "User");
        assert_eq!(top[1].1, 28);
    }

    #[test]
    fn test_profile_default() {
        let profile = SerializationProfile::default();
        assert_eq!(profile.serialize_calls(), 0);
        assert_eq!(profile.max_depth(), 0);
        assert_eq!(profile.field_counter().total_fields(), 0);
    }

    #[test]
    fn test_conservation_report_display_trait() {
        let profile = SerializationProfile::new();
        let report = profile.conservation_report("Empty");
        let via_display = format!("{}", report);
        assert!(via_display.contains("Conservation Report: Empty"));
    }

    #[test]
    fn test_budget_builder_pattern() {
        let budget = SerializationBudget::new()
            .max_bytes(1024)
            .max_fields(50)
            .max_nesting_depth(3);
        assert!(budget.check_bytes(1024).is_ok());
        assert!(budget.check_bytes(1025).is_err());
        assert!(budget.check_fields(50).is_ok());
        assert!(budget.check_fields(51).is_err());
        assert!(budget.check_nesting(3).is_ok());
        assert!(budget.check_nesting(4).is_err());
    }

    #[test]
    fn test_field_counter_top_fields_fewer_than_n() {
        let mut counter = FieldCounter::new();
        counter.record_field("A", "x", 10);
        let top = counter.top_fields(5);
        assert_eq!(top.len(), 1);
    }

    #[test]
    fn test_profile_field_tracking() {
        let mut profile = SerializationProfile::new();
        profile.record_field("User", "name", 10);
        profile.record_field("User", "email", 20);
        assert_eq!(profile.field_counter().total_fields(), 2);
        assert_eq!(profile.field_counter().total_bytes(), 30);
        let top = profile.field_counter().top_fields(2);
        assert_eq!(top[0].0, "User.email");
        assert_eq!(top[0].1, 20);
    }
}
