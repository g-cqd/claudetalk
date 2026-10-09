//! Combined flows and placement: the fake tree decides the place, and the place feeds the signature.

use super::{Tree, place_in, scratch, sig, words};
use agora_sched::signature::{place, signatures};

// Combined flows: a fake tree decides the place, and the place feeds the signature. Goldens from work.py with the
// same tree (work.place real, work.SCRATCH fixed, work.signatures(argv, cwd)).

#[test]
fn a_linked_worktree_and_its_main_checkout_share_one_signature() {
    let worktree = Tree {
        dirs: vec!["/r/main/.git"],
        files: vec![("/r/wt/.git", "gitdir: /r/main/.git/worktrees/wt\n")],
        ..Tree::default()
    };
    let main = Tree {
        dirs: vec!["/r/main/.git"],
        ..Tree::default()
    };
    let argv = words(&["swift", "test", "--parallel"]);
    let from_worktree = signatures(&argv, &place_in("/r/wt/Packages/Core", &worktree));
    let from_main = signatures(&argv, &place_in("/r/main/Packages/Core", &main));
    assert_eq!(from_worktree, from_main);
    assert_eq!(
        from_worktree,
        sig(
            "70d61d9baa1f2f42bb7e7f1e418fa93a",
            "main/Packages/Core :: swift test --parallel",
            "main/Packages/Core :: swift test",
            "swift test",
        )
    );
}

#[test]
fn a_submodule_is_signed_under_its_own_directory_name() {
    let submodule = Tree {
        files: vec![(
            "/r/outer/libs/core/.git",
            "gitdir: /r/outer/.git/modules/core\n",
        )],
        ..Tree::default()
    };
    let placed = place_in("/r/outer/libs/core/src", &submodule);
    assert_eq!(placed, "core/src");
    assert_eq!(
        signatures(&words(&["make", "test"]), &placed),
        sig(
            "fe910414fc880059063e57d95351f4b4",
            "core/src :: make test",
            "core/src :: make test",
            "make test",
        )
    );
}

#[test]
fn scratch_plain_and_checkout_directories_are_signed_with_their_digits_masked() {
    let none = Tree::default();
    let checkout = Tree {
        dirs: vec!["/Users/me/proj-12/.git"],
        ..Tree::default()
    };
    let argv = words(&["make", "test"]);
    let scratch_sig = signatures(&argv, &place_in("/tmp/build-99/x", &none));
    assert_eq!(
        scratch_sig,
        sig(
            "a7b1580f8c4cb650137ccf428c63c9b7",
            "tmp :: make test",
            "tmp :: make test",
            "make test"
        )
    );
    let plain_sig = signatures(&argv, &place_in("/Users/me/build-2024", &none));
    assert_eq!(
        plain_sig,
        sig(
            "02f82165c6ea469584094e0166a8d9ab",
            "build-# :: make test",
            "build-# :: make test",
            "make test"
        )
    );
    let digits_sig = signatures(&argv, &place_in("/Users/me/proj-12/sub3", &checkout));
    assert_eq!(
        digits_sig,
        sig(
            "3cb1e308b451f40a80dd2801bcf1cbb2",
            "proj-#/sub# :: make test",
            "proj-#/sub# :: make test",
            "make test"
        )
    );
}

// Placement alone (work.place with the fake tree, work.SCRATCH fixed).

#[test]
fn a_main_checkout_is_named_by_its_root_and_the_path_inside_it() {
    let tree = Tree {
        dirs: vec!["/Users/me/app/.git", "/Users/me/proj-12/.git"],
        ..Tree::default()
    };
    assert_eq!(
        place_in("/Users/me/app/Packages/Core", &tree),
        "app/Packages/Core"
    );
    assert_eq!(place_in("/Users/me/app", &tree), "app");
    assert_eq!(place_in("/Users/me/proj-12/sub3", &tree), "proj-#/sub#");
}

#[test]
fn a_trailing_slash_does_not_change_the_place() {
    let tree = Tree {
        dirs: vec!["/r/main/.git"],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/main/sub/", &tree), "main/sub");
}

#[test]
fn a_linked_worktree_is_named_by_its_main_checkout() {
    let tree = Tree {
        files: vec![("/r/wt/.git", "gitdir: /r/main/.git/worktrees/wt\n")],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/wt/Packages/Core", &tree), "main/Packages/Core");
    assert_eq!(place_in("/r/wt", &tree), "main");
}

#[test]
fn a_gitdir_line_ignores_padding_and_crlf_and_masks_digits_in_the_main_name() {
    let padded = Tree {
        files: vec![("/r/wt/.git", "gitdir:   /r/main/.git/worktrees/wt   \n")],
        ..Tree::default()
    };
    let crlf = Tree {
        files: vec![("/r/wt/.git", "gitdir: /r/main/.git/worktrees/wt\r\n")],
        ..Tree::default()
    };
    let digits = Tree {
        files: vec![("/r/wt/.git", "gitdir: /r/main-2/.git/worktrees/wt\n")],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/wt/x", &padded), "main/x");
    assert_eq!(place_in("/r/wt/x", &crlf), "main/x");
    assert_eq!(place_in("/r/wt/x", &digits), "main-#/x");
}

#[test]
fn an_unreadable_or_odd_git_file_names_the_checkout_by_its_directory() {
    let unreadable = Tree {
        unreadable: vec!["/r/sub/.git"],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/sub/src", &unreadable), "sub/src");
    for content in [
        "\ngitdir: /r/main/.git/worktrees/wt\n",
        "gitdir: /.git/worktrees/wt\n",
        "",
    ] {
        let tree = Tree {
            files: vec![("/r/wt/.git", content)],
            ..Tree::default()
        };
        assert_eq!(place_in("/r/wt/x", &tree), "wt/x", "content {content:?}");
    }
}

#[test]
fn a_scratch_directory_is_tmp_only_on_a_path_boundary() {
    let none = Tree::default();
    assert_eq!(place_in("/tmp/build-99/x", &none), "tmp");
    assert_eq!(place_in("/tmp", &none), "tmp");
    assert_eq!(place_in("/private/var/folders/ab/T/x", &none), "tmp");
    assert_eq!(place_in("/tmpx/y", &none), "y");
}

#[test]
fn a_plain_directory_is_its_own_name_with_digits_masked() {
    let none = Tree::default();
    assert_eq!(place_in("/Users/me/build-2024", &none), "build-#");
    assert_eq!(place_in("/Users/me/proj-12/sub3", &none), "sub#");
}

#[test]
fn no_directory_or_an_empty_one_is_none() {
    let none = Tree::default();
    assert_eq!(place(None, &none, &scratch()), "none");
    assert_eq!(place(Some(""), &none, &scratch()), "none");
}
