//! A map that enumerates in the order a .NET `Dictionary<string, T>` does, which is the order
//! TagLib# writes a Vorbis comment's fields in: a new key takes the slot the last removed key
//! freed, and only goes to the end when no slot is free.

#[derive(Debug, Clone)]
pub(crate) struct NetDictionary<V> {
    entries: Vec<Option<(String, V)>>,
    /// Freed slots, the most recently freed last (.NET's free list is a stack).
    free: Vec<usize>,
}

impl<V> Default for NetDictionary<V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<V> NetDictionary<V> {
    fn position(&self, key: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|(own, _)| own == key))
    }

    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.position(key)
            .and_then(|at| self.entries[at].as_ref().map(|(_, value)| value))
    }

    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        let at = self.position(key)?;
        self.entries[at].as_mut().map(|(_, value)| value)
    }

    /// `dictionary[key] = value`: an existing key keeps its slot; a new one takes the most
    /// recently freed slot, or goes at the end.
    pub(crate) fn insert(&mut self, key: String, value: V) {
        if let Some(at) = self.position(&key) {
            self.entries[at] = Some((key, value));
        } else if let Some(at) = self.free.pop() {
            self.entries[at] = Some((key, value));
        } else {
            self.entries.push(Some((key, value)));
        }
    }

    /// `dictionary.Remove(key)`.
    pub(crate) fn remove(&mut self, key: &str) -> bool {
        match self.position(key) {
            Some(at) => {
                self.entries[at] = None;
                self.free.push(at);
                true
            }
            None => false,
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.entries
            .iter()
            .filter_map(|entry| entry.as_ref().map(|(key, value)| (key, value)))
    }

    pub(crate) fn keys(&self) -> Vec<String> {
        self.iter().map(|(key, _)| key.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_key_takes_the_last_freed_slot() {
        let mut map = NetDictionary::default();
        for key in ["A", "B", "C", "D"] {
            map.insert(key.to_string(), ());
        }
        map.remove("B");
        map.remove("C");
        map.insert("E".into(), ());
        map.insert("F".into(), ());
        map.insert("G".into(), ());
        map.insert("A".into(), ());
        assert_eq!(map.keys(), ["A", "F", "E", "D", "G"]);
    }
}
