//! Minimal regex-lite pattern matching for skill glob patterns.
//! Avoids pulling in the full `regex` crate dependency.

pub(crate) struct Regex {
    pattern: String,
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Self, String> {
        Ok(Self {
            pattern: pattern.to_string(),
        })
    }

    pub fn is_match(&self, s: &str) -> bool {
        // Very simple: if pattern contains *, treat as wildcard
        if !self.pattern.contains('*') {
            return s == self.pattern;
        }
        let parts: Vec<&str> = self.pattern.split('*').collect();
        let mut pos = 0;
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() {
                continue;
            }
            if i == 0 && !s.starts_with(part) {
                return false;
            }
            if i == parts.len() - 1 {
                if !s[pos..].ends_with(part) {
                    return false;
                }
            } else if let Some(found) = s[pos..].find(part) {
                pos += found + part.len();
            } else {
                return false;
            }
        }
        true
    }
}
