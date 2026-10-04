//! A `Dictionary<string, T>`'s enumeration order, for the stores and lists whose order shows: a
//! state file written in that order, or a stable sort over the values. Not a C# file of Octo's;
//! the framework type those were built on.
//!
//! The order is insertion order, except that an entry added after a removal takes the most
//! recently freed slot, as `Dictionary`'s free list does.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct DotnetDictionary<T> {
    slots: Vec<Option<(String, T)>>,
    index: HashMap<String, usize>,
    /// Freed slots, the most recently freed last.
    free: Vec<usize>,
}

impl<T> Default for DotnetDictionary<T> {
    fn default() -> Self {
        DotnetDictionary {
            slots: Vec::new(),
            index: HashMap::new(),
            free: Vec::new(),
        }
    }
}

impl<T> DotnetDictionary<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&T> {
        self.index
            .get(key)
            .and_then(|&i| self.slots[i].as_ref().map(|(_, v)| v))
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut T> {
        let i = *self.index.get(key)?;
        self.slots[i].as_mut().map(|(_, v)| v)
    }

    /// `dict[key] = value`: replaces in place, or adds.
    pub fn set(&mut self, key: String, value: T) {
        if let Some(&i) = self.index.get(&key) {
            self.slots[i] = Some((key, value));
            return;
        }
        let i = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.index.insert(key.clone(), i);
        self.slots[i] = Some((key, value));
    }

    pub fn remove(&mut self, key: &str) -> Option<T> {
        let i = self.index.remove(key)?;
        self.free.push(i);
        self.slots[i].take().map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &T)> {
        self.slots.iter().filter_map(|s| s.as_ref().map(|(k, v)| (k, v)))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.iter().map(|(_, v)| v)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|s| s.as_mut().map(|(_, v)| v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_added_after_a_removal_takes_the_freed_slot() {
        let mut map = DotnetDictionary::new();
        for key in ["a", "b", "c", "d"] {
            map.set(key.to_string(), key.to_uppercase());
        }
        map.remove("b");
        map.remove("c");
        map.set("e".into(), "E".into());
        map.set("a".into(), "A2".into());
        map.set("f".into(), "F".into());
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(keys, ["a", "f", "e", "d"]);
        assert_eq!(map.get("a").map(String::as_str), Some("A2"));
        assert_eq!(map.len(), 4);
    }
}
