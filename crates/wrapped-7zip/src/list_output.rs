use {
    super::*,
    chrono::NaiveDateTime,
    std::{collections::BTreeMap, ops::Not, str::FromStr},
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ListOutputEntry {
    pub modified: chrono::NaiveDateTime,
    pub original_path: String,
    pub created: Option<chrono::NaiveDateTime>,
    pub size: u64,
    pub path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ListOutput {
    pub entries: Vec<ListOutputEntry>,
}

impl FromStr for ListOutput {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        s.trim().to_string().pipe_ref(|trimmed| {
            trimmed
                .split_once("----------")
                .context("no indicator")
                .and_then(|(_header, files)| {
                    files
                        .split("\n\n")
                        .filter_map(|entry| {
                            entry
                                .trim()
                                .pipe(|trimmed| trimmed.is_empty().not().then_some(trimmed))
                        })
                        .filter(|entry| entry.lines().count() > 2)
                        .map(|entry| {
                            entry
                                .trim()
                                .lines()
                                .map(|line| {
                                    line.split_once("=")
                                        .context("no attribute indicator (=)")
                                        .map(|(k, v)| (k.trim(), v.trim()))
                                        .context(line.to_string())
                                })
                                .collect::<Result<BTreeMap<_, _>>>()
                                .map(|e| {
                                    e.into_iter()
                                        .filter(|(_, v)| v.is_empty().not())
                                        .filter(|(_, v)| v != &"-")
                                        .collect::<BTreeMap<_, _>>()
                                })
                                .and_then(|mut entry| -> Result<_> {
                                    fn parse_date(input: &str) -> Result<NaiveDateTime> {
                                        NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M:%S").context(input.to_string())
                                    }
                                    let path = entry.remove("Path").context("no such field")?.to_string();
                                    Ok(ListOutputEntry {
                                        created: entry
                                            .remove("Created")
                                            .map(parse_date)
                                            .transpose()
                                            .context("Created")?,
                                        modified: entry
                                            .remove("Modified")
                                            .context("no such field")
                                            .and_then(parse_date)
                                            .context("Modified")?,
                                        size: entry
                                            .remove("Size")
                                            .context("no such field")
                                            .and_then(|v| v.parse().context("bad value"))
                                            .context("Size")?,
                                        original_path: path.clone(),
                                        path: path
                                            .pipe(MaybeWindowsPath)
                                            .pipe(MaybeWindowsPath::into_path),
                                    })
                                })
                                .context(entry.to_string())
                        })
                        .collect::<Result<Vec<_>>>()
                        .map(|entries| Self { entries })
                })
        })
    }
}
