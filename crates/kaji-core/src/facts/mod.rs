#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactType {
    Decision,
    Gotcha,
    Preference,
    Reference,
}

impl FactType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FactType::Decision => "decision",
            FactType::Gotcha => "gotcha",
            FactType::Preference => "preference",
            FactType::Reference => "reference",
        }
    }

    pub fn parse(s: &str) -> Option<FactType> {
        match s {
            "decision" => Some(FactType::Decision),
            "gotcha" => Some(FactType::Gotcha),
            "preference" => Some(FactType::Preference),
            "reference" => Some(FactType::Reference),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatedBy {
    Curator,
    User,
}

impl CreatedBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            CreatedBy::Curator => "curator",
            CreatedBy::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<CreatedBy> {
        match s {
            "curator" => Some(CreatedBy::Curator),
            "user" => Some(CreatedBy::User),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FactMeta {
    #[serde(rename = "type")]
    fact_type: String,
    description: String,
    date: String,
    #[serde(default)]
    session: String,
    created_by: String,
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub fact_type: FactType,
    pub slug: String,
    pub description: String,
    pub date: String,
    pub session: String,
    pub created_by: CreatedBy,
    pub body: String,
}

impl Fact {
    pub fn file_name(&self) -> String {
        format!("{}-{}.md", self.fact_type.as_str(), self.slug)
    }

    pub fn to_markdown(&self) -> String {
        let meta = FactMeta {
            fact_type: self.fact_type.as_str().to_string(),
            description: self.description.clone(),
            date: self.date.clone(),
            session: self.session.clone(),
            created_by: self.created_by.as_str().to_string(),
        };
        format!(
            "---\n{}---\n\n{}\n",
            serde_yaml::to_string(&meta).expect("frontmatter serialization"),
            self.body.trim_end()
        )
    }

    pub fn parse(file_name: &str, content: &str) -> Option<Fact> {
        let rest = content.strip_prefix("---\n")?;
        let close = rest.find("\n---\n")?;
        let frontmatter = &rest[..close];
        let body = rest[close + "\n---\n".len()..].trim().to_string();

        let meta: FactMeta = serde_yaml::from_str(frontmatter).ok()?;
        let fact_type = FactType::parse(&meta.fact_type)?;
        let created_by = CreatedBy::parse(&meta.created_by)?;

        let slug = file_name
            .strip_suffix(".md")?
            .strip_prefix(&format!("{}-", fact_type.as_str()))?
            .to_string();
        if !validate_slug(&slug) {
            return None;
        }

        Some(Fact {
            fact_type,
            slug,
            description: meta.description,
            date: meta.date,
            session: meta.session,
            created_by,
            body,
        })
    }
}

pub fn validate_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn slugify(text: &str) -> String {
    let mut mapped = String::new();
    for c in text.chars() {
        for lower in c.to_lowercase() {
            if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
                mapped.push(lower);
            } else {
                mapped.push('-');
            }
        }
    }

    let mut collapsed = String::new();
    let mut prev_dash = false;
    for c in mapped.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push('-');
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }

    collapsed.trim_matches('-').chars().take(64).collect()
}
