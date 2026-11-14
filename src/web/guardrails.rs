//! Operator rule sets and attachments. Pasted test text is not stored.
//!
//! The rule editor posts one group of fields per rule:
//! `rule[<n>][name|kind|detector|pattern|words|whole_word|case_sensitive|action|direction|remove]`.
//! The field order is the rule order; `<n>` only groups the fields. A text
//! in `rules` (a JSON array) replaces the rows: scripts and the import box
//! use it.
use super::*;
use crate::guardrails::{
    Action, DETECTORS, Direction, Engine, Kind, MAX_RULES, MAX_TEXT_LIMIT, Rule, RuleError,
    RuleSet, compile_rule, valid_hook_url,
};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/guardrails", get(page).post(save))
        .route("/admin/guardrails/test", post(test))
        .route("/admin/guardrails/{id}/delete", post(delete))
        .route("/admin/guardrails/{id}/attach", post(attach))
        .route("/admin/guardrails/attachments/{id}/delete", post(detach))
}

/// Empty rows after the saved rules, for new rules without JavaScript.
const EMPTY_ROWS: usize = 2;
/// Rows that one form can send, empty rows included.
const MAX_ROWS: usize = 2 * MAX_RULES;

#[derive(sqlx::FromRow)]
struct Set {
    id: i64,
    name: String,
    rules: String,
    hook_url: String,
    fail_closed: bool,
    max_text_bytes: i64,
}
impl Set {
    fn config(&self) -> anyhow::Result<RuleSet> {
        Ok(RuleSet {
            name: self.name.clone(),
            rules: serde_json::from_str(&self.rules)?,
            hook_url: self.hook_url.clone(),
            fail_closed: self.fail_closed,
            max_text_bytes: usize::try_from(self.max_text_bytes)?,
        })
    }
}

/// A detector for the select: value, label and a one-line description.
pub(super) struct Detector {
    value: &'static str,
    label: &'static str,
    about: &'static str,
}
const DETECTOR_INFO: [Detector; 5] = [
    Detector {
        value: "email",
        label: "Email address",
        about: "Finds addresses such as name@example.com.",
    },
    Detector {
        value: "phone",
        label: "Phone number",
        about: "Finds a plus sign followed by 7 to 15 digits, such as +442079460958.",
    },
    Detector {
        value: "credit_card",
        label: "Credit card number",
        about: "Finds 13 to 19 digits that pass the Luhn check. Spaces and hyphens are allowed.",
    },
    Detector {
        value: "iban",
        label: "IBAN",
        about: "Finds bank account numbers in IBAN format that pass the checksum.",
    },
    Detector {
        value: "secret",
        label: "Secret or API key",
        about: "Finds OpenAI, Anthropic, AWS, GitHub, Slack and Google keys, private keys and JWTs.",
    },
];

/// One row of the rule editor: the typed values, as text.
#[derive(Clone, Default)]
pub(super) struct Row {
    /// Groups the fields of the row: `rule[<key>][...]`.
    key: String,
    name: String,
    kind: String,
    detector: String,
    pattern: String,
    /// One keyword per line.
    words: String,
    whole_word: bool,
    case_sensitive: bool,
    action: String,
    direction: String,
    remove: bool,
    /// An empty row for the no-JS form. The JS removes it.
    extra: bool,
    /// The sub-field with an error ("name", "pattern", ...), or empty.
    bad: &'static str,
}
impl Row {
    fn blank(key: usize, extra: bool) -> Self {
        Self {
            key: key.to_string(),
            kind: "detector".into(),
            action: "redact".into(),
            direction: "both".into(),
            extra,
            ..Default::default()
        }
    }
    fn from_rule(key: usize, rule: &Rule) -> Self {
        let mut row = Self {
            key: key.to_string(),
            name: rule.name.clone(),
            action: action_str(rule.action).into(),
            direction: direction_str(rule.direction).into(),
            ..Default::default()
        };
        match &rule.kind {
            Kind::Detector { detector } => {
                row.kind = "detector".into();
                row.detector = detector.clone();
            }
            Kind::Regex { pattern } => {
                row.kind = "regex".into();
                row.pattern = pattern.clone();
            }
            Kind::Keywords {
                words,
                whole_word,
                case_sensitive,
            } => {
                row.kind = "keywords".into();
                row.words = words.join("\n");
                row.whole_word = *whole_word;
                row.case_sensitive = *case_sensitive;
            }
        }
        row
    }
    /// An untouched row: it is not a rule.
    fn is_empty(&self) -> bool {
        [&self.name, &self.detector, &self.pattern, &self.words]
            .iter()
            .all(|v| v.trim().is_empty())
    }
    fn words(&self) -> Vec<String> {
        self.words
            .lines()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect()
    }
    /// The one-line description of the selected detector.
    fn about(&self) -> &'static str {
        DETECTOR_INFO
            .iter()
            .find(|d| d.value == self.detector)
            .map_or("Select a detector.", |d| d.about)
    }
}

/// The values of one rule set form: saved, or typed before an error.
pub(super) struct Editor {
    /// "new" for the add form, else the rule set id. Element ids use it.
    key: String,
    id: Option<i64>,
    /// The saved name, for the heading.
    title: String,
    name: String,
    max_text_bytes: String,
    hook_url: String,
    fail_closed: bool,
    rows: Vec<Row>,
    /// The saved rules as JSON, for export. Empty for the add form.
    json: String,
    /// The typed text of the import box.
    import: String,
    /// An error for this form, and the top-level field ("name", ...) it is about.
    error: String,
    bad: &'static str,
    /// The attachments of a saved rule set.
    attached: Vec<Attachment>,
}
impl Editor {
    fn new_set() -> Self {
        Self {
            key: "new".into(),
            id: None,
            title: String::new(),
            name: String::new(),
            max_text_bytes: crate::guardrails::RuleSet::default()
                .max_text_bytes
                .to_string(),
            hook_url: String::new(),
            fail_closed: false,
            rows: (0..=EMPTY_ROWS).map(|i| Row::blank(i, i > 0)).collect(),
            json: String::new(),
            import: String::new(),
            error: String::new(),
            bad: "",
            attached: vec![],
        }
    }
    fn saved(set: &Set) -> Self {
        let rules: Vec<Rule> = serde_json::from_str(&set.rules).unwrap_or_default();
        let mut rows: Vec<Row> = rules
            .iter()
            .enumerate()
            .map(|(i, r)| Row::from_rule(i, r))
            .collect();
        let n = rows.len();
        rows.extend((n..n + EMPTY_ROWS).map(|i| Row::blank(i, true)));
        Self {
            key: set.id.to_string(),
            id: Some(set.id),
            title: set.name.clone(),
            name: set.name.clone(),
            max_text_bytes: set.max_text_bytes.to_string(),
            hook_url: set.hook_url.clone(),
            fail_closed: set.fail_closed,
            rows,
            json: serde_json::to_string_pretty(&rules).unwrap_or_else(|_| set.rules.clone()),
            import: String::new(),
            error: String::new(),
            bad: "",
            attached: vec![],
        }
    }
    /// The row template for the JS: placeholders for the form and row keys.
    fn template() -> (Self, Row) {
        let mut ed = Self::new_set();
        ed.key = "__F__".into();
        ed.rows.clear();
        let mut row = Row::blank(0, false);
        row.key = "__I__".into();
        (ed, row)
    }
    /// Read the posted fields. Rows keep the order of their first field.
    fn from_pairs(pairs: Vec<(String, String)>) -> Result<Self, Response> {
        let mut ed = Self::new_set();
        ed.rows.clear();
        ed.max_text_bytes.clear();
        for (k, v) in pairs {
            match k.as_str() {
                "id" if !v.is_empty() => match v.parse() {
                    Ok(id) => {
                        ed.id = Some(id);
                        ed.key = v;
                    }
                    Err(_) => return Err(form_error("Reload the page and try again.")),
                },
                "name" => ed.name = v,
                "max_text_bytes" => ed.max_text_bytes = v,
                "hook_url" => ed.hook_url = v,
                "fail_closed" => ed.fail_closed = true,
                "rules" => ed.import = v,
                _ => {
                    let Some((key, field)) = rule_key(&k) else {
                        continue;
                    };
                    let i = match ed.rows.iter().position(|r| r.key == key) {
                        Some(i) => i,
                        None if ed.rows.len() < MAX_ROWS => {
                            ed.rows.push(Row {
                                key: key.into(),
                                ..Default::default()
                            });
                            ed.rows.len() - 1
                        }
                        None => return Err(too_many()),
                    };
                    let row = &mut ed.rows[i];
                    match field {
                        "name" => row.name = v,
                        "kind" => row.kind = v,
                        "detector" => row.detector = v,
                        "pattern" => row.pattern = v,
                        "words" => row.words = v,
                        "whole_word" => row.whole_word = true,
                        "case_sensitive" => row.case_sensitive = true,
                        "action" => row.action = v,
                        "direction" => row.direction = v,
                        "remove" => row.remove = true,
                        _ => {}
                    }
                }
            }
        }
        Ok(ed)
    }
}

/// `rule[12][name]` -> ("12", "name").
fn rule_key(k: &str) -> Option<(&str, &str)> {
    let (key, field) = k
        .strip_prefix("rule[")?
        .strip_suffix(']')?
        .split_once("][")?;
    (!key.is_empty() && key.len() <= 4 && key.bytes().all(|b| b.is_ascii_digit()))
        .then_some((key, field))
}

fn too_many() -> Response {
    form_error(format!(
        "Use at most {MAX_RULES} rules in a rule set. Remove rules, or put them in a second rule set."
    ))
}

fn action_str(a: Action) -> &'static str {
    match a {
        Action::Block => "block",
        Action::Redact => "redact",
        Action::Log => "log",
    }
}
fn direction_str(d: Direction) -> &'static str {
    match d {
        Direction::Request => "request",
        Direction::Response => "response",
        Direction::Both => "both",
    }
}

/// A validation error: the message, and the field it is about.
struct Problem {
    /// Index in `Editor::rows`, for a row field.
    row: Option<usize>,
    field: &'static str,
    message: String,
}
impl Problem {
    fn top(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            row: None,
            field,
            message: message.into(),
        }
    }
    fn row(row: usize, field: &'static str, message: impl Into<String>) -> Self {
        Self {
            row: Some(row),
            field,
            message: message.into(),
        }
    }
}

/// "Rule 2" or "Rule 2 (EMAIL)".
fn label(n: usize, name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        format!("Rule {n}")
    } else {
        format!("Rule {n} ({name})")
    }
}

/// The form-only checks of a row, then the rule.
fn row_rule(n: usize, row: &Row) -> Result<Rule, (&'static str, String)> {
    let name = row.name.trim();
    let l = label(n, name);
    if name.is_empty() {
        return Err(("name", format!("Rule {n}: enter a name.")));
    }
    let kind = match row.kind.as_str() {
        "detector" => {
            if !DETECTORS.contains(&row.detector.as_str()) {
                return Err(("detector", format!("{l}: select a detector.")));
            }
            Kind::Detector {
                detector: row.detector.clone(),
            }
        }
        "regex" => {
            if row.pattern.is_empty() {
                return Err(("pattern", format!("{l}: enter a regex pattern.")));
            }
            Kind::Regex {
                pattern: row.pattern.clone(),
            }
        }
        "keywords" => Kind::Keywords {
            words: row.words(),
            whole_word: row.whole_word,
            case_sensitive: row.case_sensitive,
        },
        _ => return Err(("kind", format!("{l}: select Detector, Regex or Keywords."))),
    };
    let action = match row.action.as_str() {
        "block" => Action::Block,
        "redact" => Action::Redact,
        "log" => Action::Log,
        _ => return Err(("action", format!("{l}: select an action."))),
    };
    let direction = match row.direction.as_str() {
        "request" => Direction::Request,
        "response" => Direction::Response,
        "both" => Direction::Both,
        _ => return Err(("direction", format!("{l}: select a direction."))),
    };
    Ok(Rule {
        name: name.into(),
        kind,
        action,
        direction,
    })
}

/// Duplicate names and compile errors, with the same limits as the Proxy.
/// Returns the index of the rule, the field and the end of the message.
fn check_rules(rules: &[Rule]) -> Result<(), (usize, &'static str, String)> {
    for (i, rule) in rules.iter().enumerate() {
        if let Some(j) = rules[..i].iter().position(|r| r.name == rule.name) {
            return Err((
                i,
                "name",
                format!("Rule {} has the same name. Use a different name.", j + 1),
            ));
        }
        let keywords = matches!(rule.kind, Kind::Keywords { .. });
        if let Kind::Keywords { words, .. } = &rule.kind
            && words.is_empty()
        {
            return Err((
                i,
                "words",
                "enter at least one keyword, one per line.".into(),
            ));
        }
        if let Err(e) = compile_rule(rule) {
            let (field, text) = match e {
                RuleError::Name => (
                    "name",
                    "use a name with 1 to 100 bytes, without control characters.",
                ),
                RuleError::UnknownDetector => ("detector", "select a detector."),
                RuleError::Keywords => (
                    "words",
                    "use 1 to 256 keywords, one per line, without empty keywords.",
                ),
                RuleError::PatternTooLong | RuleError::PatternInvalid if keywords => (
                    "words",
                    "the keywords are too long together. Use fewer or shorter keywords.",
                ),
                RuleError::PatternTooLong => {
                    ("pattern", "the regex pattern is longer than 8192 bytes.")
                }
                RuleError::PatternInvalid => (
                    "pattern",
                    "the regex pattern is not valid, or it is too large. Check the brackets, parentheses, repetitions and escapes.",
                ),
                RuleError::MatchesEmpty => (
                    "pattern",
                    "the regex pattern matches empty text. Change it so that each match has at least one character.",
                ),
            };
            return Err((i, field, text.into()));
        }
    }
    Ok(())
}

/// Validate a posted form into a rule set.
fn validate(ed: &Editor) -> Result<RuleSet, Problem> {
    let name = ed.name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return Err(Problem::top(
            "name",
            "Enter a rule set name with 1 to 100 bytes, without control characters.",
        ));
    }
    let rules = if ed.import.trim().is_empty() {
        let live: Vec<usize> = (0..ed.rows.len())
            .filter(|&i| !ed.rows[i].remove && !ed.rows[i].is_empty())
            .collect();
        if live.len() > MAX_RULES {
            return Err(Problem::top(
                "",
                format!(
                    "Use at most {MAX_RULES} rules in a rule set. Remove rules, or put them in a second rule set."
                ),
            ));
        }
        let mut rules = Vec::with_capacity(live.len());
        for &i in &live {
            // Rule numbers count all rows, as the legends do.
            rules.push(row_rule(i + 1, &ed.rows[i]).map_err(|(f, m)| Problem::row(i, f, m))?);
        }
        check_rules(&rules).map_err(|(r, f, m)| {
            let i = live[r];
            Problem::row(i, f, format!("{}: {m}", label(i + 1, &rules[r].name)))
        })?;
        rules
    } else {
        let rules: Vec<Rule> = serde_json::from_str(&ed.import).map_err(|_| {
            Problem::top(
                "rules",
                "Use a JSON array of rules in the import box, in the format of the saved JSON. Or clear the import box to save the rules above.",
            )
        })?;
        if rules.len() > MAX_RULES {
            return Err(Problem::top(
                "rules",
                format!("Use at most {MAX_RULES} rules in the JSON."),
            ));
        }
        check_rules(&rules).map_err(|(r, _, m)| {
            Problem::top(
                "rules",
                format!("{} in the JSON: {m}", label(r + 1, &rules[r].name)),
            )
        })?;
        rules
    };
    let max_text_bytes = ed
        .max_text_bytes
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=MAX_TEXT_LIMIT).contains(n))
        .ok_or_else(|| {
            Problem::top(
                "max_text_bytes",
                format!("Enter a Text limit from 1 to {MAX_TEXT_LIMIT} bytes."),
            )
        })?;
    let hook_url = ed.hook_url.trim();
    if !valid_hook_url(hook_url) {
        return Err(Problem::top(
            "hook_url",
            "Use an http:// or https:// External moderation URL without a user name or password, or leave it empty.",
        ));
    }
    Ok(RuleSet {
        name: name.into(),
        rules,
        hook_url: hook_url.into(),
        fail_closed: ed.fail_closed,
        max_text_bytes,
    })
}

#[derive(sqlx::FromRow)]
struct Attachment {
    id: i64,
    set_id: i64,
    target: String,
}
#[derive(Template)]
#[template(path = "guardrails.html")]
struct Page {
    base: Base,
    /// The saved rule sets, then the add form (`id` is None).
    forms: Vec<Editor>,
    tpl_ed: Editor,
    tpl_row: Row,
    detectors: &'static [Detector],
    routes: Option<Vec<db::ControlChoice>>,
    teams: Option<Vec<db::ControlChoice>>,
    keys: Vec<db::KeyRow>,
}

/// The page, with the typed values of a failed save in its form.
async fn render_page(st: &AppState, draft: Option<Editor>) -> Response {
    let sets: Vec<Set> = tryr!(
        sqlx::query_as("SELECT * FROM guardrail_sets ORDER BY name LIMIT 64")
            .fetch_all(&st.db)
            .await
    );
    let attachments: Vec<Attachment> = tryr!(sqlx::query_as("SELECT a.id,a.set_id,CASE WHEN a.route IS NOT NULL THEN 'Route: '||a.route WHEN a.team_id IS NOT NULL THEN 'Team: '||t.name ELSE 'Virtual Key: '||k.label||' (#'||k.id||')' END AS target FROM guardrail_attachments a LEFT JOIN teams t ON t.id=a.team_id LEFT JOIN virtual_keys k ON k.id=a.key_id ORDER BY a.id LIMIT 1000").fetch_all(&st.db).await);
    let mut add = Editor::new_set();
    let mut sets: Vec<Editor> = sets.iter().map(Editor::saved).collect();
    for a in attachments {
        if let Some(ed) = sets.iter_mut().find(|e| e.id == Some(a.set_id)) {
            ed.attached.push(a);
        }
    }
    if let Some(mut draft) = draft {
        match draft.id {
            None => add = draft,
            Some(id) => {
                if let Some(ed) = sets.iter_mut().find(|e| e.id == Some(id)) {
                    draft.title = std::mem::take(&mut ed.title);
                    draft.json = std::mem::take(&mut ed.json);
                    draft.attached = std::mem::take(&mut ed.attached);
                    *ed = draft;
                }
            }
        }
    }
    sets.push(add);
    let (tpl_ed, tpl_row) = Editor::template();
    let mut r = render(Page {
        base: Base::new(st),
        forms: sets,
        tpl_ed,
        tpl_row,
        detectors: &DETECTOR_INFO,
        routes: tryr!(db::small_choices(&st.db, "routes").await),
        teams: tryr!(db::small_choices(&st.db, "teams").await),
        keys: tryr!(db::list_keys(&st.db).await),
    });
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn page(_: Admin, State(st): State<AppState>) -> Response {
    render_page(&st, None).await
}

/// An error for a save: next to the field for htmx, else the page again
/// with the typed values, the message and the field marked.
async fn reject(st: &AppState, htmx: bool, mut ed: Editor, p: Problem) -> Response {
    if htmx {
        let field = match p.row {
            Some(i) => format!("rule[{}][{}]", ed.rows[i].key, p.field),
            None => p.field.to_owned(),
        };
        return if field.is_empty() {
            form_error(p.message)
        } else {
            field_error(&field, p.message)
        };
    }
    match p.row {
        Some(i) => ed.rows[i].bad = p.field,
        None => ed.bad = p.field,
    }
    ed.error = p.message;
    let mut r = render_page(st, Some(ed)).await;
    if r.status().is_success() {
        *r.status_mut() = StatusCode::BAD_REQUEST;
    }
    r
}

async fn save(
    _: Admin,
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let htmx = headers.contains_key("hx-request");
    let ed = match Editor::from_pairs(pairs) {
        Ok(ed) => ed,
        Err(r) => return r,
    };
    let set = match validate(&ed) {
        Ok(set) => set,
        Err(p) => return reject(&st, htmx, ed, p).await,
    };
    if Engine::new(vec![set.clone()]).is_err() {
        return form_error(
            "Check the rule names, patterns, detector names, text limit and hook URL. Patterns must not match empty text.",
        );
    }
    let name = set.name.as_str();
    let rules = tryr!(serde_json::to_string_pretty(&set.rules));
    let _guard = st.settings_write.lock().await;
    let result = if let Some(id) = ed.id {
        sqlx::query("UPDATE guardrail_sets SET name=?,rules=?,hook_url=?,fail_closed=?,max_text_bytes=? WHERE id=?").bind(name).bind(rules).bind(&set.hook_url).bind(set.fail_closed).bind(set.max_text_bytes as i64).bind(id).execute(&st.db).await
    } else {
        let count: i64 = tryr!(
            sqlx::query_scalar("SELECT count(*) FROM guardrail_sets")
                .fetch_one(&st.db)
                .await
        );
        if count >= 64 {
            return form_error("Delete a rule set before you add another. The limit is 64.");
        }
        sqlx::query("INSERT INTO guardrail_sets(name,rules,hook_url,fail_closed,max_text_bytes) VALUES(?,?,?,?,?)").bind(name).bind(rules).bind(&set.hook_url).bind(set.fail_closed).bind(set.max_text_bytes as i64).execute(&st.db).await
    };
    if let Err(e) = result {
        let e = e.into();
        drop(_guard);
        return if is_unique_violation(&e) {
            reject(
                &st,
                htmx,
                ed,
                Problem::top("name", "This rule set name is in use. Use another name."),
            )
            .await
        } else {
            internal(e)
        };
    }
    let path = match ed.id {
        Some(id) => format!("/admin/guardrails#set-{id}"),
        None => "/admin/guardrails".into(),
    };
    saved(&path, "Guardrail rule set saved.")
}
async fn delete(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM guardrail_sets WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/guardrails", "Guardrail rule set deleted.")
}
#[derive(Deserialize)]
struct AttachForm {
    #[serde(default)]
    route_name: String,
    #[serde(default)]
    team_name: String,
    #[serde(default)]
    key_id: String,
}
async fn attach(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<AttachForm>,
) -> Response {
    let route = f.route_name.trim();
    let team = f.team_name.trim();
    let key = f.key_id.trim();
    if [!route.is_empty(), !team.is_empty(), !key.is_empty()]
        .into_iter()
        .filter(|v| *v)
        .count()
        != 1
    {
        return form_error("Select one Route, Team or Virtual Key.");
    }
    let team_id: Option<i64> = if team.is_empty() {
        None
    } else {
        let found = tryr!(
            sqlx::query_scalar("SELECT id FROM teams WHERE name=?")
                .bind(team)
                .fetch_optional(&st.db)
                .await
        );
        if found.is_none() {
            return form_error("Select a listed Team.");
        }
        found
    };
    let key_id: Option<i64> = if key.is_empty() {
        None
    } else {
        match key.parse() {
            Ok(v) => Some(v),
            Err(_) => return form_error("Select a listed Virtual Key."),
        }
    };
    let result = sqlx::query(
        "INSERT INTO guardrail_attachments(set_id,route,team_id,key_id) VALUES(?,?,?,?)",
    )
    .bind(id)
    .bind((!route.is_empty()).then_some(route))
    .bind(team_id)
    .bind(key_id)
    .execute(&st.db)
    .await;
    if result.is_err() {
        return form_error("Select a listed rule set and a listed Route, Team or Virtual Key.");
    }
    saved(
        &format!("/admin/guardrails#set-{id}"),
        "Guardrail attachment added.",
    )
}
async fn detach(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM guardrail_attachments WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/guardrails", "Guardrail attachment deleted.")
}
#[derive(Deserialize)]
struct TestForm {
    set_id: i64,
    text: String,
    direction: Direction,
}
/// The matches of one rule in a test.
struct Hit {
    rule: String,
    action: &'static str,
    count: usize,
    /// "0 to 17, 22 to 39", at most 10 ranges.
    spans: String,
}
#[derive(Template)]
#[template(path = "guardrail_test.html")]
struct TestResult {
    verdict: crate::guardrails::Verdict,
    hits: Vec<Hit>,
}
async fn test(_: Admin, State(st): State<AppState>, Form(f): Form<TestForm>) -> Response {
    let set: Option<Set> = tryr!(
        sqlx::query_as("SELECT * FROM guardrail_sets WHERE id=?")
            .bind(f.set_id)
            .fetch_optional(&st.db)
            .await
    );
    let Some(set) = set else {
        return form_error("Select a listed rule set.");
    };
    if f.direction == Direction::Both {
        return form_error("Select request or response.");
    }
    let mut config = tryr!(set.config());
    // A test must not send pasted text to an external service.
    config.hook_url.clear();
    let engine = tryr!(Engine::new(vec![config]));
    let verdict = engine.inspect(f.direction, &f.text).await;
    let mut hits: Vec<(Hit, Vec<String>)> = vec![];
    for m in &verdict.matches {
        let i = match hits.iter().position(|(h, _)| h.rule == m.rule) {
            Some(i) => i,
            None => {
                hits.push((
                    Hit {
                        rule: m.rule.clone(),
                        action: action_str(m.action),
                        count: 0,
                        spans: String::new(),
                    },
                    vec![],
                ));
                hits.len() - 1
            }
        };
        let (hit, spans) = &mut hits[i];
        hit.count += 1;
        if spans.len() < 10 {
            spans.push(format!("{} to {}", m.start, m.end));
        }
    }
    let hits = hits
        .into_iter()
        .map(|(mut h, spans)| {
            h.spans = spans.join(", ");
            if h.count > spans.len() {
                h.spans
                    .push_str(&format!(" and {} more", h.count - spans.len()));
            }
            h
        })
        .collect();
    Html(tryr!(TestResult { verdict, hits }.render())).into_response()
}
