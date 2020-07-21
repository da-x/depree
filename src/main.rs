use derive_error::Error;
use im::{Vector, OrdMap};
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;
use std::rc::Rc;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
struct Opt {
    #[structopt(subcommand)]
    cmd: Command
}

#[derive(StructOpt, Debug)]
enum Command {
    Version,
    VerifyRebaseInteractive { script_file: PathBuf, },
}

#[derive(Debug, Error)]
pub enum Error {
    VarEnv(std::env::VarError),
    Io(std::io::Error),
    UniDiff(unidiff::Error),
    Git2(git2::Error),
    NonMonotonicPatchLines,
    NotScriptFile,
}

type LineNr = usize;

#[derive(Clone, Copy, Debug)]
enum FileKind {
    Addition,
    Changes,
    Deletion,
}

#[derive(Clone, Default, Debug)]
struct Hunk {
    source: Vector<Rc<String>>,
    target: Vector<Rc<String>>,
}

#[derive(Clone, Debug)]
struct FileInfo {
    kind: FileKind,
    hunks: Vector<(LineNr, Hunk)>,
}

type GitRef = String;

#[derive(Clone, Debug)]
struct ChangeSet {
    githash: GitRef,
    files: OrdMap<String, FileInfo>,
}

fn add_commit_text(githash: &Option<String>, lines: &String) -> Result<ChangeSet, Error> {
    let empty_line = Rc::new(String::new());
    if let Some(githash) = githash {
        let mut files = OrdMap::new();

        let mut ps = unidiff::PatchSet::new();
        ps.parse(lines)?;

        for file in ps.files() {
            let (normalization, kind) = if file.is_added_file() {
                (0, FileKind::Addition)
            } else if file.is_removed_file() {
                (0, FileKind::Deletion)
            } else {
                (1, FileKind::Changes)
            };

            let mut hunks = Vector::new();

            for hunk in file.hunks() {
                let mut new_hunk = Hunk::default();
                for line in hunk.lines() {
                    match line.line_type.as_ref() {
                        unidiff::LINE_TYPE_ADDED => {
                            let rc = Rc::new(line.value.clone());
                            new_hunk.target.push_back(rc);
                        }
                        unidiff::LINE_TYPE_REMOVED => {
                            let rc = Rc::new(line.value.clone());
                            new_hunk.source.push_back(rc);
                        }
                        unidiff::LINE_TYPE_CONTEXT => {
                            let rc = Rc::new(line.value.clone());
                            new_hunk.source.push_back(rc.clone());
                            new_hunk.target.push_back(rc);
                        }
                        unidiff::LINE_TYPE_EMPTY => {
                            new_hunk.source.push_back(
                                empty_line.clone());
                            new_hunk.target.push_back(
                                empty_line.clone());
                        }
                        _ => panic!(),
                    }
                }
                hunks.push_back((hunk.source_start - normalization, new_hunk));
            }

            files.insert(file.path(), FileInfo {
                kind,
                hunks,
            });
        }

        return Ok(ChangeSet {
            githash: githash.to_owned(),
            files,
        });
    }

    panic!();
}

#[derive(Debug)]
enum MergeError {
    UnappliedHunk(u32),
}

#[derive(Debug)]
struct MergeErrors {
    list: Vec<(String, MergeError)>,
}

enum FileState {
    Oid(git2::Oid),
    Removed,
    Loaded(Vector<Rc<String>>),
}

type FileSet = HashMap<String, FileState>;

fn apply_hunks(file_info: &FileInfo, content: &mut Vector<Rc<String>>) -> Result<(), MergeError> {
    let mut source_diff = 0isize;

    for (source_line, hunk) in &file_info.hunks {
        let pivot_line = (*source_line as isize).saturating_add(source_diff) as usize;
        let mut distance = 0isize;
        let mut fuzz = None;

        'found: while distance < content.len() as isize {
            let two_places = [distance, -distance];
            let places = if distance == 0 {
                &[0][..]
            } else {
                &two_places[..]
            };

            'next: for place in places.into_iter() {
                let v = if *place < 0 {
                    if pivot_line as isize >= -*place {
                        Some((pivot_line as isize + *place) as usize)
                    } else {
                        None
                    }
                } else if *place > 0 {
                    if pivot_line + *place as usize +
                        hunk.source.len() > content.len()
                    {
                        None
                    } else {
                        Some(pivot_line + *place as usize)
                    }
                } else {
                    Some(pivot_line)
                };

                if let Some(v) = v {
                    for line in 0 ..  hunk.source.len() {
                        if hunk.source[line] != content[line + v] {
                            continue 'next;
                        }
                    }

                    fuzz = Some(v as isize - pivot_line as isize);
                    break 'found;
                }
            }

            distance += 1;
        }

        let pos = match fuzz {
            None => {
                return Err(MergeError::UnappliedHunk(*source_line as u32));
            }
            Some(v) => {
                (v + pivot_line as isize) as usize
            }
        };

        let t_l = hunk.target.len();
        let s_l = hunk.source.len();

        let part = content.split_off(pos);
        let (_, after) = part.split_at(s_l);
        content.extend(hunk.target.clone());
        content.extend(after);

        source_diff += t_l as isize;
        source_diff -= s_l as isize;
    }

    Ok(())
}

fn apply(repo: &git2::Repository, fs: &mut FileSet, changeset: &ChangeSet) -> Result<(), MergeErrors>
{
    let mut merge_errors = MergeErrors {
        list: vec![],
    };

    for (path, file_info) in &changeset.files {
        println!("  {}", path);
        if let Some(file) = fs.get_mut(path) {
            match &file {
                FileState::Oid(oid) => {
                    let blob = repo.find_blob(*oid);
                    let content = blob.as_ref().unwrap().content();
                    let mut vector = Vector::new();
                    for line in content.lines() {
                        vector.push_back(Rc::new(String::from(line.unwrap())))
                    }
                    *file = FileState::Loaded(vector);
                }
                _ => {}
            }
        }

        match file_info.kind {
            FileKind::Addition => {
                for hunk in &file_info.hunks {
                    if let Some(file) = fs.get_mut(path) {
                        match &file {
                            FileState::Oid(_) => panic!(),
                            FileState::Loaded(_) => todo!(),
                            FileState::Removed => {
                                *file = FileState::Loaded(hunk.1.target.clone());
                            }
                        }
                    }
                }
            }
            FileKind::Deletion => {
                if let Some(file) = fs.get_mut(path) {
                    *file = FileState::Removed;
                }
            }
            FileKind::Changes => {
                if let Some(mut file) = fs.get_mut(path) {
                    match &mut file {
                        FileState::Oid(_) => panic!(),
                        FileState::Removed => todo!(),
                        FileState::Loaded(content) => {
                            if let Err(err) = apply_hunks(&file_info, content) {
                                merge_errors.list.push((path.clone(), err));
                            }
                        }
                    }
                }
            }
        }
    }

    if merge_errors.list.len() > 0 {
        return Err(merge_errors);
    }

    Ok(())
}

fn commit_to_fileset(obj: git2::Object) -> Result<FileSet, Error> {
    let mut blobs = std::collections::HashMap::new();

    if let Some(parent_commit) = obj.as_commit() {
        if let Ok(tree) = parent_commit.tree() {
            tree.walk(git2::TreeWalkMode::PreOrder, |v, entry| {
                if let Some(name) = entry.name() {
                    if let Some(git2::ObjectType::Blob) = entry.kind() {
                        let path = format!("{}{}", v, name);
                        blobs.insert(path, FileState::Oid(entry.id()));
                    }
                }
                git2::TreeWalkResult::Ok
            })?;
        }
    }

    Ok(blobs)
}

fn verify_rebase_interactive(script_path: &PathBuf) -> Result<(), Error> {
    let suffix = "/rebase-merge/git-rebase-todo";
    let onto_suffix = "/rebase-merge/onto";
    let script_path_str = script_path.to_str().unwrap();
    if !script_path_str.ends_with(suffix) {
        return Err(Error::NotScriptFile);
    }

    lazy_static! {
        static ref RE: Regex = Regex::new("^ *(pick|reword|squash|fixup) ([^ ]+)").unwrap();
    }

    let repo_path = &script_path_str[..script_path_str.len() - suffix.len()];
    let rebase_onto = std::fs::read_to_string(&(repo_path.to_owned() + onto_suffix))?;
    let rebase_onto = rebase_onto.trim();
    let repo = git2::Repository::open(&repo_path)?;
    let obj = repo.revparse_single(&rebase_onto)?;
    let mut fileset = commit_to_fileset(obj)?;

    let mut commits = vec![];
    for (line_nr, line) in std::io::BufReader::new(std::fs::File::open(script_path_str)?).lines().enumerate() {
        if let Some(p) = RE.captures(&line?) {
            if let Some(commit_hash) = p.get(2) {
                let xgithash = String::from(commit_hash.as_str());
                let obj = repo.revparse_single(&xgithash)?;
                let tree = obj.peel_to_tree()?;
                let commit = obj.peel_to_commit();

                if let Ok(commit) = commit {
                    let parents : Vec<_> = commit.parents().collect();
                    let parent = parents.first().unwrap();
                    let parent_tree = parent.tree()?;

                    let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)?;
                    let mut s = Vec::new();

                    diff.print(git2::DiffFormat::Patch, |_, _, l| {
                        match l.origin() {
                            '+' | '-' | ' ' => s.push(l.origin() as u8),
                            _ => {}
                        }
                        s.extend(l.content());
                        true
                    })?;

                    let diff_text = String::from_utf8_lossy(&s).into_owned();
                    let githash = Some(xgithash);
                    commits.push((line_nr + 1, add_commit_text(&githash, &diff_text)?));
                }
            }
        }
    }

    let nr_commits = commits.len();
    for (index, (line_nr, commit)) in commits.into_iter().enumerate() {
        println!("Processing [{}/{}]: {}", index + 1, nr_commits, commit.githash);

        match apply(&repo, &mut fileset, &commit) {
            Err(err) => {
                println!("{}:{}: error: {:?}", script_path_str, line_nr, err);
                break;
            }
            Ok(()) => {}
        }
    }

    Ok(())
}

fn main() -> Result<(), Error> {
    let opt = Opt::from_args();

    match &opt.cmd {
        Command::Version => {
            println!("{}", env!("VERGEN_SHA"));
        }
        Command::VerifyRebaseInteractive { script_file } => {
            verify_rebase_interactive(script_file)?;
        }
    }

    Ok(())
}
