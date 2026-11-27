use std::process::Command;

use super::*;
use crate::test_dir::TempDir;

fn setup(name: &str, files: &[(&str, &str)]) -> (TempDir, Vault, Index) {
    let dir = TempDir::new(name);
    for (p, t) in files {
        dir.write(&format!("vault/{p}"), t);
    }
    let vault = Vault::open(dir.0.join("vault")).unwrap();
    let mut index = Index::open(&vault).unwrap();
    index.sync(&vault).unwrap();
    (dir, vault, index)
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn repository_turns_wiki_links_into_relative_markdown_links() {
    let (d, vault, index) = setup(
        "gh-repo",
        &[
            (
                "a.md",
                "# A\n[[b]] [[sub/c|see c]] [[#Top Part]] [[gone]] [[c#Some Heading]]\n\
                 `[[code]]`\n| x |\n|-|\n| [[b\\|cell]] |\n[md](b.md)\n",
            ),
            ("b.md", "# B\n"),
            ("sub/c.md", "# C\n[[a]]\n![i](../img/p.png)\n"),
            ("img/p.png", "png"),
            (".hidden/n.md", "[[a]]"),
        ],
    );
    let out = d.0.join("out");
    let report = export(&vault, &index, &out, Style::Repository).unwrap();
    assert_eq!(
        read(&out, "a.md"),
        "# A\n[b](b.md) [see c](sub/c.md) [#Top Part](#top-part) gone \
         [c#Some Heading](sub/c.md#some-heading)\n\
         `[[code]]`\n| x |\n|-|\n| [cell](b.md) |\n[md](b.md)\n"
    );
    assert_eq!(
        read(&out, "sub/c.md"),
        "# C\n[a](../a.md)\n![i](../img/p.png)\n"
    );
    assert_eq!(read(&out, "img/p.png"), "png");
    assert!(!out.join(".hidden").exists() && !out.join(".katib").exists());
    assert_eq!(
        report,
        Report {
            notes: 3,
            files: 1,
            unlinked: vec![Unlinked {
                id: "a".into(),
                link: "[[gone]]".into()
            }],
        }
    );
}

#[test]
fn wiki_flattens_notes_and_links_from_the_root() {
    let (d, vault, index) = setup(
        "gh-wiki",
        &[
            (
                "README.md",
                "[[notes/Twin]] [[Twin]] [[proj]] [link](proj/README.md) ![i](img/p.png)\n",
            ),
            ("Twin.md", "![i](img/p.png) [r](README.md)\n"),
            (
                "notes/Twin.md",
                "![i](../img/p.png) [[My Note#Part Two]] [def][d] [x](../none.md)\n\n[d]: ../Twin.md\n",
            ),
            (
                "notes/My Note.md",
                "---\ntitle: Mine\ntags: [a]\n---\n\nx [[Twin]]\n",
            ),
            ("notes/bare.md", "---\ntags: [a]\n---\nx\n"),
            ("proj/README.md", "[[Twin]]\n"),
            ("img/p.png", "png"),
        ],
    );
    let out = d.0.join("out");
    let report = export(&vault, &index, &out, Style::Wiki).unwrap();
    assert_eq!(
        read(&out, "Home.md"),
        "[notes/Twin](notes-Twin) [Twin](Twin) [proj](proj) [link](proj) ![i](img/p.png)\n"
    );
    assert_eq!(read(&out, "Twin.md"), "![i](img/p.png) [r](Home)\n");
    assert_eq!(
        read(&out, "notes-Twin.md"),
        "![i](img/p.png) [My Note#Part Two](My-Note#part-two) [def][d] [x](../none.md)\n\n[d]: Twin\n"
    );
    assert_eq!(read(&out, "My-Note.md"), "# Mine\n\nx [Twin](Twin)\n");
    assert_eq!(read(&out, "bare.md"), "x\n");
    assert_eq!(read(&out, "proj.md"), "[Twin](Twin)\n");
    assert_eq!(read(&out, "img/p.png"), "png");
    assert!(!out.join("notes").exists() && !out.join("proj").exists());
    assert_eq!((report.notes, report.files), (6, 1));
    assert_eq!(
        report.unlinked,
        [Unlinked {
            id: "notes/Twin".into(),
            link: "[x](../none.md)".into()
        }]
    );
}

#[test]
fn page_names_are_unique_ignoring_case() {
    let names = page_names(&[
        "README",
        "home",
        "a/x",
        "b/X",
        "dir",
        "dir/README",
        "n/My Note",
    ]);
    let got = |id: &str| names[id].as_str();
    assert_eq!(got("README"), "Home");
    assert_eq!(got("home"), "home-2");
    assert_eq!((got("a/x"), got("b/X")), ("a-x", "b-X"));
    assert_eq!((got("dir"), got("dir/README")), ("dir", "dir-README"));
    assert_eq!(got("n/My Note"), "My-Note");
}

#[test]
fn destination_inside_the_vault_only_under_a_dot_folder() {
    let (_d, vault, index) = setup("gh-inside", &[("a.md", "x")]);
    let inside = vault.root().join("out");
    let err = export(&vault, &index, &inside, Style::Repository).unwrap_err();
    assert!(matches!(
        err,
        ExportError::Destination(_, "is inside the vault")
    ));
    assert!(!inside.exists());
    let hidden = vault.root().join(".katib/out/github");
    export(&vault, &index, &hidden, Style::Repository).unwrap();
    assert_eq!(read(&hidden, "a.md"), "x");
}

#[test]
fn destination_with_files_and_no_git_is_refused() {
    let (d, vault, index) = setup("gh-full", &[("a.md", "x")]);
    let out = d.0.join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("keep.txt"), "mine").unwrap();
    let err = export(&vault, &index, &out, Style::Repository).unwrap_err();
    assert!(matches!(err, ExportError::Destination(..)));
    assert_eq!(read(&out, "keep.txt"), "mine");
    assert!(!out.join("a.md").exists());
}

#[test]
fn clean_git_destination_is_replaced_and_dirty_one_refused() {
    let (d, vault, index) = setup("gh-git", &[("a.md", "x")]);
    let out = d.0.join("out");
    fs::create_dir_all(out.join("old")).unwrap();
    fs::write(out.join("old/stale.md"), "gone").unwrap();
    git(&out, &["init", "-q"]);
    git(&out, &["add", "-A"]);
    git(&out, &["commit", "-qm", "old"]);

    fs::write(out.join("old/stale.md"), "edited").unwrap();
    let err = export(&vault, &index, &out, Style::Wiki).unwrap_err();
    assert!(matches!(
        err,
        ExportError::Destination(_, "has uncommitted or ignored files")
    ));
    assert_eq!(read(&out, "old/stale.md"), "edited");

    git(&out, &["checkout", "-q", "--", "."]);
    export(&vault, &index, &out, Style::Wiki).unwrap();
    assert!(!out.join("old").exists());
    assert!(out.join(".git").is_dir());
    assert_eq!(read(&out, "a.md"), "x");
}

#[test]
fn wiki_without_a_root_readme_gets_a_contents_page() {
    let (d, vault, index) = setup(
        "gh-home",
        &[("b.md", "# Bee [x]\n"), ("sub/a.md", "plain\n")],
    );
    let out = d.0.join("out");
    let report = export(&vault, &index, &out, Style::Wiki).unwrap();
    assert_eq!(
        read(&out, "Home.md"),
        "# Contents\n\n- [Bee [x\\]](b)\n- [a](a)\n"
    );
    assert_eq!(report.notes, 2);

    // A note that takes the name `Home` is the front page instead.
    let (d2, vault2, index2) = setup("gh-home-note", &[("x/home.md", "mine\n")]);
    let out = d2.0.join("out");
    export(&vault2, &index2, &out, Style::Wiki).unwrap();
    let names: Vec<_> = fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["home.md"]);
}
