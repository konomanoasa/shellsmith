use super::super::fixtures;
use super::Application;
use crate::interface::application::{Error, engine};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn prepare(app: &mut Application, cancelled: &AtomicBool) -> Result<(), Error> {
  app.prepare_workspace_analysis(cancelled)?;
  app.restore_analysis_documents(cancelled)
}

fn dependencies(
  app: &Application,
  id: engine::DocumentId,
) -> impl Iterator<Item = (&engine::SourceReference, engine::DependencyState)> {
  let states = engine::AnalysisResults::new(
    app
      .analyses
      .iter()
      .flat_map(|collection| collection.entries().unwrap().values()),
  )
  .dependencies();
  app
    .workspace
    .document(id)
    .into_iter()
    .flat_map(engine::Document::references)
    .filter_map(move |reference| {
      states
        .get(&reference.id)
        .cloned()
        .map(|state| (reference, state))
    })
}

mod documents {
  use super::fixtures;
  use super::{dependencies, prepare};
  use crate::interface::application::engine::{
    DependencyState, Modifier, SemanticKind,
  };
  use crate::interface::application::{Application, Error, Settings};
  use fixtures::Fixture;
  use std::sync::atomic::AtomicBool;

  #[test]
  fn invalid_source_bytes_preserve_other_links_and_recover_after_repair() {
    let fixture = Fixture::new();
    let good = fixture.write("good.sh", "good=known\n");
    let main = fixture.write(
      "main.sh",
      &format!(
        ". ./good.sh\n. ./broken.sh\n. '{}'\nkept=after\n",
        good.display()
      ),
    );
    let broken = fixture.write("broken.sh", "");
    std::fs::write(&broken, [0xff]).unwrap();
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      crate::interface::application::engine::InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.add_input(&main);
    let cancelled = AtomicBool::new(false);
    prepare(&mut app, &cancelled).unwrap();
    let id = app.document_id(&main).unwrap();
    assert_eq!(
      app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [good.clone(), good.clone()]
    );
    assert_eq!(app.failures().len(), 1);
    assert_eq!(app.failures()[0].path, broken);
    assert!(app.failures()[0].message.contains("Invalid UTF-8"));
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"good")
        .count(),
      1
    );
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"kept")
        .count(),
      1
    );
    fixture.write("broken.sh", "repaired=known\n");
    prepare(&mut app, &cancelled).unwrap();
    assert_eq!(app.failures(), []);
    assert_eq!(
      app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [good.clone(), broken, good]
    );
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"repaired")
        .count(),
      1
    );
  }

  #[test]
  fn symbol_features_separate_document_scope_from_workspace_name_search() {
    let fixture = Fixture::new();
    let main =
      fixture.write("main.sh", "local_name=1\nmain_function() { :; }\n");
    let library = fixture.write("lib.sh", "library_name=2\n");
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let symbols = app
      .document_symbols(id)
      .unwrap()
      .map(|entry| {
        assert_eq!(entry.details.location.snapshot.document, id);
        String::from_utf8(entry.details.symbol.name.clone()).unwrap()
      })
      .collect::<Vec<_>>();
    assert_eq!(symbols, ["local_name", "main_function"]);
    let matches = app
      .workspace_symbols("name")
      .map(|entry| String::from_utf8(entry.symbol.name.clone()).unwrap())
      .collect::<Vec<_>>();
    assert_eq!(matches, ["library_name", "local_name"]);
    assert_eq!(app.workspace_symbols("missing").count(), 0);
    let removed = app.document_id(&library).unwrap();
    std::fs::remove_file(library).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert!(
      matches!(app.document_symbols(removed), Err(Error::Engine(crate::interface::application::engine::Error::UnknownDocument(missing))) if missing == removed)
    );
    assert_eq!(app.workspace_symbols("library").count(), 0);
  }

  #[test]
  fn opened_text_replaces_unreadable_files_while_closed_ones_remain_failures() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", "value=1; : \"$value\"");
    let locked = fixture.write("locked.sh", "disk=1");
    let notes = fixture.write("notes", "text");
    let permissions = std::fs::Permissions::from_mode;
    for path in [&locked, &notes] {
      std::fs::set_permissions(path, permissions(0o000)).unwrap();
    }
    if std::fs::read(&locked).is_ok() {
      // Permissions do not restrict this user.
      return;
    }
    let failed = |app: &Application| {
      app
        .failures()
        .iter()
        .map(|failure| failure.path.clone())
        .collect::<Vec<_>>()
    };
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    super::overlay(&mut app, &locked, Some("live=1"), &cancel).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(failed(&app), [notes.as_path()]);
    assert!(matches!(
      app.prepare_rename(&main, 0, &cancel),
      Err(Error::RenameRefused(_))
    ));
    std::fs::set_permissions(&notes, permissions(0o644)).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.failures(), []);
    assert_eq!(app.document_id(&notes), None);
    let id = app.document_id(&locked).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "live=1");
    assert!(app.prepare_rename(&main, 0, &cancel).unwrap().is_some());
    super::overlay(&mut app, &locked, None, &cancel).unwrap();
    assert!(app.failures().iter().any(|failure| failure.path == locked));
    assert_eq!(app.document_id(&locked), None);
    assert!(matches!(
      app.prepare_rename(&main, 0, &cancel),
      Err(Error::RenameRefused(_))
    ));
    assert_eq!(failed(&app), [locked]);
  }

  #[test]
  fn ignored_documents_remain_available_when_opened_selected_or_sourced() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "ignored/\n");
    let main = fixture.write("main.sh", ". ./ignored/lib.sh\n");
    let library = fixture.write("ignored/lib.sh", "library=1\n");
    let opened = fixture.write("ignored/open.sh", "disk=1\n");
    let selected = fixture.write("ignored/explicit.txt", "explicit=1\n");
    let skipped = fixture.write("ignored/skipped.sh", "skipped=1\n");
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    let cancel = AtomicBool::new(false);
    app.add_input(&fixture.root);
    app.add_input(&selected);
    super::overlay(&mut app, &opened, Some("live=1\n"), &cancel).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 4);
    assert!(app.document_id(&skipped).is_none());
    let id = app.document_id(&opened).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "live=1\n");
    let id = app.document_id(&main).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert!(app.document_id(&library).is_some());
    assert!(app.document_id(&selected).is_some());
    assert_eq!(app.failures(), []);
  }

  #[test]
  fn overlays_close_and_disk_failures_never_reuse_stale_results() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "disk=1");
    let cancel = AtomicBool::new(false);
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&file);
    super::overlay(&mut app, &file, Some("live=1"), &cancel).unwrap();
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&file).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "live=1");
    std::fs::write(&file, "changed=1").unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "live=1");
    super::overlay(&mut app, &file, None, &cancel).unwrap();
    assert_eq!(app.workspace().document(id).unwrap().source(), "changed=1");
    std::fs::write(&file, [0xff]).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert!(app.workspace().document(id).is_none());
    assert_eq!(app.failures().len(), 1);
    fixture.write("main.sh", "fixed=1");
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.document_id(&file), Some(id));
    std::fs::remove_file(&file).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert!(app.document_id(&file).is_none());
  }

  #[test]
  fn sources_follow_entry_directory_and_cycles_with_current_revisions() {
    let fixture = Fixture::new();
    let entry = fixture.write("sub/main.sh", ". ./lib.sh\nf\n");
    let lib = fixture.write("sub/lib.sh", "f() { :; }\n. ./main.sh\n");
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.add_input(&entry);
    let cancel = AtomicBool::new(false);
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&entry).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert_eq!(app.workspace().documents().count(), 2);
    super::overlay(&mut app, &lib, Some("f() { :; }"), &cancel).unwrap();
    prepare(&mut app, &cancel).unwrap();
    let spans = app.classify(id, &cancel).unwrap();
    assert!(spans.iter().any(|span| span.kind == SemanticKind::Function));
    super::overlay(&mut app, &lib, Some("g() { :; }"), &cancel).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert!(
      !app
        .classify(id, &cancel)
        .unwrap()
        .iter()
        .any(|span| span.kind == SemanticKind::Function)
    );
    super::overlay(&mut app, &lib, None, &cancel).unwrap();
    std::fs::remove_file(&lib).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Unresolved(_)
    ));
  }

  #[test]
  fn failed_cd_keeps_the_current_directory_for_source_resolution() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "cd sub; . ./lib.sh\n");
    let library = fixture.write("lib.sh", "readonly x=1");
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 2);
    let id = app.document_id(&file).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert_eq!(app.document_links(id, &cancel).unwrap()[0].1, library);
    assert!(
      app
        .classify(id, &cancel)
        .unwrap()
        .iter()
        .any(|span| span.modifiers.contains(&Modifier::DefaultLibrary))
    );
  }

  #[test]
  fn cancellation_does_not_publish_partial_analysis() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "x=1");
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&file);
    prepare(&mut app, &AtomicBool::new(false)).unwrap();
    let id = app.document_id(&file).unwrap();
    app.classify(id, &AtomicBool::new(false)).unwrap();
    let completed =
      std::ptr::from_ref(app.workspace().analysis().unwrap()).addr();
    assert!(app.classify(id, &AtomicBool::new(true)).is_err());
    assert_eq!(
      std::ptr::from_ref(app.workspace().analysis().unwrap()).addr(),
      completed
    );
    super::overlay(&mut app, &file, Some("x=2"), &AtomicBool::new(false))
      .unwrap();
    assert!(app.classify(id, &AtomicBool::new(true)).is_err());
    assert!(app.workspace().analysis().is_none());
  }

  #[cfg(unix)]
  #[test]
  fn symbolic_links_share_documents_and_directory_cycles_terminate() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", "x=1");
    let link = fixture.root.join("link.sh");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    prepare(&mut app, &AtomicBool::new(false)).unwrap();
    assert_eq!(app.workspace().documents().count(), 1);
    std::os::unix::fs::symlink(&fixture.root, fixture.root.join("cycle"))
      .unwrap();
    app.add_input(&link);
    prepare(&mut app, &AtomicBool::new(false)).unwrap();
    assert_eq!(app.failures(), []);
    assert_eq!(app.document_id(&link), app.document_id(&file));
    super::overlay(&mut app, &link, Some("x=2"), &AtomicBool::new(false))
      .unwrap();
    prepare(&mut app, &AtomicBool::new(false)).unwrap();
    assert_eq!(app.workspace().documents().count(), 1);
  }

  #[cfg(unix)]
  #[test]
  fn overlays_keep_their_identity_after_the_path_becomes_a_link() {
    let fixture = Fixture::new();
    let original = fixture.write("main.sh", "disk=1");
    let other = fixture.write("other.sh", "other=1");
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    let identity = app.overlay_path(&original, true).unwrap();
    app.set_overlay(identity.clone(), Some(std::sync::Arc::from("opened=1")));
    std::fs::remove_file(&original).unwrap();
    std::os::unix::fs::symlink(&other, &original).unwrap();
    assert_eq!(app.overlay_path(&original, true).unwrap(), identity);
    app.set_overlay(identity.clone(), Some(std::sync::Arc::from("edited=1")));
    assert_eq!(app.opened[&identity].as_ref(), "edited=1");
    assert!(!app.opened.contains_key(&other));
    assert_eq!(app.overlay_path(&original, false).unwrap(), identity);
    app.set_overlay(identity.clone(), None);
    assert!(!app.opened.contains_key(&identity));
    assert_eq!(app.overlay_path(&original, false).unwrap(), other);
  }

  #[test]
  fn unsaved_source_and_path_entries_use_the_request_directory() {
    let fixture = Fixture::new();
    let entry = fixture.write("main.sh", ". lib\nf\n");
    let library = fixture.root.join("lib");
    let mut settings = Settings::new(fixture.root.clone());
    settings
      .initial
      .environment
      .insert(b"PATH".to_vec(), b"./missing:".to_vec());
    let mut app = Application::new(settings).unwrap();
    let cancelled = AtomicBool::new(false);
    app.add_input(&entry);
    super::overlay(&mut app, &library, Some("f() { :; }"), &cancelled).unwrap();
    prepare(&mut app, &cancelled).unwrap();
    let id = app.document_id(&entry).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    assert!(
      app
        .classify(id, &cancelled)
        .unwrap()
        .iter()
        .any(|span| span.kind == SemanticKind::Function)
    );
    let mut settings = app.settings().clone();
    settings.initial.environment.remove(b"PATH".as_slice());
    app.settings = settings;
    app.invalidate_inputs();
    assert!(app.workspace().analysis().is_none());
    prepare(&mut app, &cancelled).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
  }

  #[test]
  fn sourced_cd_changes_the_directory_for_later_references() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "cd elsewhere\n");
    let entry = fixture.write(
      "main.sh",
      &format!(". '{}'\n. ./next.sh\n", library.display()),
    );
    fixture.write("next.sh", "wrong=1");
    fixture.write("elsewhere/next.sh", "right=1");
    let settings = Settings::new(fixture.root.clone());
    let mut app = Application::new(settings).unwrap();
    app.add_input(&entry);
    prepare(&mut app, &AtomicBool::new(false)).unwrap();
    let id = app.document_id(&entry).unwrap();
    let states: Vec<_> =
      dependencies(&app, id).map(|(_, state)| state).collect();
    assert!(matches!(states[0], DependencyState::Resolved(_)));
    assert!(matches!(states[1], DependencyState::Resolved(_)));
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"wrong")
        .count(),
      0
    );
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"right")
        .count(),
      1
    );
  }

  #[cfg(unix)]
  #[test]
  fn parent_components_are_resolved_after_symbolic_links() {
    let fixture = Fixture::new();
    let file = fixture.write("real/a.sh", "x=1");
    std::fs::create_dir(fixture.root.join("real/sub")).unwrap();
    std::os::unix::fs::symlink(
      fixture.root.join("real/sub"),
      fixture.root.join("link"),
    )
    .unwrap();
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root.join("link/../a.sh"));
    prepare(&mut app, &AtomicBool::new(false)).unwrap();
    assert_eq!(app.failures(), []);
    assert!(app.document_id(&file).is_some());
  }

  #[test]
  fn source_search_uses_only_the_conditions_required_by_the_path() {
    for (prefix, resolved) in [
      (": \"$(( $expr ))\"", true),
      ("getopts x PATH", true),
      ("local PATH=elsewhere", false),
      ("fc", false),
    ] {
      let fixture = Fixture::new();
      let file = fixture.write("main.sh", &format!("{prefix}; . ./lib.sh\n"));
      fixture.write("lib.sh", "x=1");
      let settings = Settings::new(fixture.root.clone());
      let mut app = Application::new(settings).unwrap();
      app.add_input(&file);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      let id = app.document_id(&file).unwrap();
      assert_eq!(
        matches!(
          dependencies(&app, id).next().unwrap().1,
          DependencyState::Resolved(_)
        ),
        resolved,
        "{prefix}"
      );
    }
  }

  #[test]
  fn entry_directories_are_independent_and_sources_inherit_directory_changes() {
    let fixture = Fixture::new();
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial =
      crate::interface::application::engine::InitialEnvironment::empty();
    settings
      .initial
      .environment
      .insert(b"PATH".to_vec(), b".:".to_vec());
    let mut app = Application::new(settings).unwrap();
    let mut expected = Vec::new();
    for (directory, operand) in
      [("a", "\"$PWD/lib/first.sh\""), ("b", "./lib/first.sh")]
    {
      let source = format!(". {operand}\nf\n. done.sh\n");
      let main = fixture.write(&format!("{directory}/main.sh"), &source);
      let first = fixture.write(
        &format!("{directory}/lib/first.sh"),
        ". ./second.sh\ncd ./moved\n",
      );
      let second =
        fixture.write(&format!("{directory}/second.sh"), "f() { :; }\n");
      let done =
        fixture.write(&format!("{directory}/moved/done.sh"), "done=1\n");
      super::overlay(&mut app, &main, Some(&source), &cancel).unwrap();
      expected.push((main, first, second, done));
    }
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.failures(), []);
    assert_eq!(app.workspace().documents().count(), 8);
    for (main, first, second, done) in expected {
      let id = app.document_id(&main).unwrap();
      let links = app.document_links(id, &cancel).unwrap();
      assert_eq!(
        links.into_iter().map(|(_, path)| path).collect::<Vec<_>>(),
        vec![first.clone(), done]
      );
      let source = app.workspace().document(id).unwrap().source();
      let offset = source.find("\nf\n").unwrap() + 1;
      let definitions = app.definitions(id, offset, &cancel).unwrap();
      assert_eq!(definitions.len(), 1);
      assert_eq!(
        app.path(definitions[0].snapshot.document),
        Some(second.as_path())
      );
      assert!(
        app
          .classify(id, &cancel)
          .unwrap()
          .iter()
          .any(|span| { span.kind == SemanticKind::Function })
      );
    }
  }
}

mod environment {
  use super::fixtures;
  use super::{dependencies, prepare};

  use crate::interface::application::engine::{
    DependencyState, InitialBinding, Modifier,
  };
  use crate::interface::application::{Application, Settings};
  use fixtures::Fixture;
  use std::sync::atomic::AtomicBool;

  fn application(fixture: &Fixture) -> Application {
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial.environment.clear();
    settings.initial.environment.insert(
      b"HOME".to_vec(),
      fixture.root.as_os_str().as_encoded_bytes().to_vec(),
    );
    Application::new(settings).unwrap()
  }

  #[test]
  fn missing_startup_variables_resolve_defaults_until_their_values_become_unknown()
   {
    let fixture = Fixture::new();
    fixture.write("default.sh", ":\n");
    fixture.write("assigned.sh", ":\n");
    for (prefix, operand, target) in [
      ("", "${CONFIG-./default.sh}", Some("default.sh")),
      ("", "${CONFIG:-./default.sh}", Some("default.sh")),
      ("", "${CONFIG:=./default.sh}", Some("default.sh")),
      ("CONFIG=; ", "${CONFIG-./default.sh}", None),
      ("CONFIG=; ", "${CONFIG:-./default.sh}", Some("default.sh")),
      ("CONFIG=./assigned.sh; ", "$CONFIG", Some("assigned.sh")),
      (
        "if condition; then unset CONFIG; fi; ",
        "${CONFIG:-./default.sh}",
        Some("default.sh"),
      ),
      (
        "if condition; then CONFIG=./assigned.sh; fi; ",
        "${CONFIG:-./default.sh}",
        None,
      ),
      ("read CONFIG; ", "${CONFIG:-./default.sh}", None),
      (
        "read CONFIG; unset CONFIG; ",
        "${CONFIG:-./default.sh}",
        Some("default.sh"),
      ),
      (
        "read CONFIG; CONFIG=./assigned.sh; ",
        "$CONFIG",
        Some("assigned.sh"),
      ),
    ] {
      let source = format!("{prefix}. \"{operand}\"\n");
      let main = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      prepare(&mut app, &cancelled).unwrap();
      let id = app.document_id(&main).unwrap();
      let targets: Vec<_> = app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect();
      assert_eq!(
        targets,
        target
          .map(|path| fixture.root.join(path))
          .into_iter()
          .collect::<Vec<_>>(),
        "{source}"
      );
    }
  }

  #[test]
  fn explicit_initial_uncertainty_is_not_replaced_with_unset() {
    let fixture = Fixture::new();
    fixture.write("default.sh", ":\n");
    for name in [
      "CONFIG",
      "LANG",
      "LC_ALL",
      "LC_CTYPE",
      "LC_COLLATE",
      "CDPATH",
    ] {
      let main =
        fixture.write("main.sh", &format!(". \"${{{name}-./default.sh}}\"\n"));
      let mut app = application(&fixture);
      let mut settings = app.settings().clone();
      settings.initial.other_variables = InitialBinding::Unknown;
      app.settings = settings;
      app.invalidate_inputs();
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      prepare(&mut app, &cancelled).unwrap();
      let id = app.document_id(&main).unwrap();
      assert!(
        app.document_links(id, &cancelled).unwrap().is_empty(),
        "{name}"
      );
      assert_eq!(app.workspace().documents().count(), 1, "{name}");
    }
  }

  #[test]
  fn shell_initialized_variables_skip_unset_defaults_and_held_ones_are_set() {
    let fixture = Fixture::new();
    fixture.write("default.sh", ":\n");
    for (name, held) in [
      ("LINENO", true),
      ("PPID", true),
      ("PS1", false),
      ("PS2", false),
      ("PS4", false),
    ] {
      for (source, linked) in [
        (format!(". \"${{{name}-./default.sh}}\"\n"), false),
        (format!(". \"${{{name}+./default.sh}}\"\n"), held),
      ] {
        let main = fixture.write("main.sh", &source);
        let mut app = application(&fixture);
        app.add_input(&main);
        let cancelled = AtomicBool::new(false);
        prepare(&mut app, &cancelled).unwrap();
        let id = app.document_id(&main).unwrap();
        assert_eq!(
          app.document_links(id, &cancelled).unwrap().len(),
          usize::from(linked),
          "{source}"
        );
        assert_eq!(
          app.workspace().documents().count(),
          1 + usize::from(linked),
          "{source}"
        );
      }
    }
  }

  #[test]
  fn quoted_environment_sources_are_loaded_and_feed_semantic_classification() {
    let fixture = Fixture::new();
    let file = fixture.write(
      "main.sh",
      ". ./prefix.sh; . \"$HOME/lib.sh\"; f; : \"${HOME}\"\n",
    );
    fixture.write("prefix.sh", "prefix=1\n");
    fixture.write("lib.sh", "f() { :; }\nreadonly HOME\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 3);
    let id = app.document_id(&file).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Resolved(_)
    ));
    let spans = app.classify(id, &cancel).unwrap();
    assert!(spans.iter().any(|span| span.kind
      == crate::interface::application::engine::SemanticKind::Function));
    assert_eq!(
      spans
        .iter()
        .filter(|span| span.modifiers.contains(&Modifier::Environment))
        .count(),
      2
    );
    assert!(
      spans
        .iter()
        .any(|span| span.modifiers.contains(&Modifier::Readonly)
          && span.modifiers.contains(&Modifier::Environment))
    );
  }

  #[test]
  fn source_operand_uses_the_environment_before_command_prefix_assignments() {
    let fixture = Fixture::new();
    let file = fixture.write(
      "main.sh",
      "HOME=/other . \"$HOME/lib.sh\"; . \"$HOME/after.sh\"\n",
    );
    fixture.write("lib.sh", "loaded=1\n");
    fixture.write("after.sh", "unreachable=1\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancelled = AtomicBool::new(false);
    prepare(&mut app, &cancelled).unwrap();
    let id = app.document_id(&file).unwrap();
    let states: Vec<_> =
      dependencies(&app, id).map(|(_, state)| state).collect();
    assert!(matches!(states[0], DependencyState::Resolved(_)));
    assert!(matches!(states[1], DependencyState::Unresolved(_)));
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"loaded")
        .count(),
      1
    );
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"unreachable")
        .count(),
      0
    );
  }

  #[test]
  fn source_discovery_never_uses_overwritten_or_uncertain_environment_values() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "from_library=1\n");
    for (prefix, library_count) in [
      ("HOME=/other; ", 0),
      ("unset HOME; ", 0),
      ("if condition; then HOME=/other; fi; ", 1),
      ("read HOME; ", 0),
      ("f() { HOME=/other; }; f; ", 0),
      (". missing; ", 0),
    ] {
      let file = fixture.write(
        "main.sh",
        &format!("{prefix}. \"$HOME/lib.sh\"\n: \"$from_library\"\n"),
      );
      let mut app = application(&fixture);
      app.add_input(&file);
      let cancelled = AtomicBool::new(false);
      prepare(&mut app, &cancelled).unwrap();
      let id = app.document_id(&file).unwrap();
      assert!(
        app.document_links(id, &cancelled).unwrap().is_empty(),
        "{prefix}"
      );
      assert!(
        dependencies(&app, id)
          .all(|(_, state)| matches!(state, DependencyState::Unresolved(_))),
        "{prefix}"
      );
      assert_eq!(
        app
          .workspace()
          .documents()
          .filter(|doc| app.path(doc.snapshot().document).is_some())
          .count(),
        1 + library_count,
        "{prefix}"
      );
      assert_eq!(
        app
          .workspace()
          .symbols()
          .filter(|symbol| symbol.name == b"from_library")
          .count(),
        library_count,
        "{prefix}"
      );
      let source = app.workspace().document(id).unwrap().source();
      let offset = source.rfind("from_library").unwrap();
      assert!(
        app.definitions(id, offset, &cancelled).unwrap().is_empty(),
        "{prefix}"
      );
    }
  }

  #[test]
  fn excluded_eval_and_alias_effects_do_not_block_later_source_discovery() {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "f() { :; }\nreadonly HOME\n");
    let hidden = fixture.write("hidden.sh", "hidden=1\n");
    for call in [
      "eval '. ./hidden.sh; cd /; PATH=/other; HOME=/other'",
      "eval \"$(brew shellenv)\"",
      "alias .='echo'; alias f=printf",
      "alias .='echo'; unalias .",
    ] {
      let source = format!("{call}; . \"$HOME/lib.sh\"; f; : \"$HOME\"");
      let file = fixture.write("main.sh", &source);
      let mut app = application(&fixture);
      app.add_input(&file);
      let cancel = AtomicBool::new(false);
      prepare(&mut app, &cancel).unwrap();
      assert!(app.document_id(&library).is_some(), "{call}");
      assert!(app.document_id(&hidden).is_none(), "{call}");
      assert_eq!(app.workspace().documents().count(), 2, "{call}");
      let id = app.document_id(&file).unwrap();
      let spans = app.classify(id, &cancel).unwrap();
      let function = source.rfind("f; :").unwrap();
      assert!(
        spans
          .iter()
          .any(|span| span.location.bytes.start == function
            && span.kind
              == crate::interface::application::engine::SemanticKind::Function),
        "{call}"
      );
      let read = source.rfind("HOME").unwrap();
      assert!(
        spans.iter().any(|span| span.location.bytes.start == read
          && span.modifiers.contains(&Modifier::Environment)
          && span.modifiers.contains(&Modifier::Readonly)),
        "{call}"
      );
    }
  }

  #[test]
  fn nested_sources_recheck_environment_after_each_file_and_after_updates() {
    let fixture = Fixture::new();
    let file =
      fixture.write("main.sh", ". \"$HOME/first.sh\"; . \"$HOME/last.sh\"\n");
    let first = fixture.write("first.sh", ". \"${HOME}/nested.sh\"\n");
    fixture.write("nested.sh", "nested=1\n");
    let last = fixture.write("last.sh", "last=1\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 4);
    assert!(app.document_id(&last).is_some());
    std::fs::write(&first, "unset HOME\n").unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.workspace().documents().count(), 2);
    assert!(app.document_id(&last).is_none());
    std::fs::write(&first, ":\n").unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert!(app.document_id(&last).is_some());
  }

  #[test]
  fn quoted_paths_preserve_spaces_defaults_and_unsupported_expansions() {
    let fixture = Fixture::new();
    fixture.write("with space/lib.sh", "found=1\n");
    for (operand, expected) in [
      ("\"$HOME/with space/lib.sh\"", 2),
      ("\"${HOME}/with space/\"'lib.sh'", 2),
      ("\"${HOME:-fallback}/with space/lib.sh\"", 2),
      ("\"${#HOME}/with space/lib.sh\"", 1),
      ("$HOME/with\\ space/lib.sh", 2),
      ("'${HOME}/with space/lib.sh'", 1),
      ("\"$HOME/with space/\"*.sh", 2),
      ("\"$(echo ignored)/lib.sh\"", 1),
    ] {
      let file = fixture.write("main.sh", &format!(". {operand}\n"));
      let mut app = application(&fixture);
      app.add_input(&file);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      assert_eq!(app.workspace().documents().count(), expected, "{operand}");
    }
  }

  #[test]
  fn function_and_loop_contexts_cannot_share_an_unproven_source_target() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "HOME=/changed\n");
    for source in [
      "f() { . \"$HOME/lib.sh\"; }; f; f\n",
      "while condition; do . \"$HOME/lib.sh\"; done\n",
    ] {
      let file = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      app.add_input(&file);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      let id = app.document_id(&file).unwrap();
      assert!(
        dependencies(&app, id)
          .all(|(_, state)| matches!(state, DependencyState::Unresolved(_)))
      );
      assert_eq!(app.workspace().documents().count(), 2);
      assert_eq!(app.document_links(id, &AtomicBool::new(false)).unwrap(), []);
    }
  }

  #[test]
  fn workspace_entries_keep_shared_source_contexts_separate() {
    let fixture = Fixture::new();
    let first = fixture.write("first.sh", ". \"$HOME/shared.sh\"\n");
    let second =
      fixture.write("second.sh", "HOME=/other; . \"$CONFIG/shared.sh\"\n");
    let shared = fixture.write("shared.sh", ". \"$HOME/leaf.sh\"\n");
    fixture.write("leaf.sh", "leaf=1\n");
    let mut app = application(&fixture);
    let mut settings = app.settings().clone();
    settings.initial.environment.insert(
      b"CONFIG".to_vec(),
      fixture.root.as_os_str().as_encoded_bytes().to_vec(),
    );
    app.settings = settings;
    app.invalidate_inputs();
    app.add_input(&first);
    app.add_input(&second);
    prepare(&mut app, &AtomicBool::new(false)).unwrap();
    let id = app.document_id(&shared).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Unresolved(_)
    ));
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"leaf")
        .count(),
      1
    );
  }

  #[test]
  fn environment_changes_invalidate_discovery() {
    let fixture = Fixture::new();
    let file = fixture.write("main.sh", ". \"${CONFIG-./lib.sh}\"\n");
    let first = fixture.write("lib.sh", "old=1\n");
    let second = fixture.write("other/lib.sh", "new=1\n");
    let mut app = application(&fixture);
    app.add_input(&file);
    let cancel = AtomicBool::new(false);
    for (value, target) in [
      (None, Some(&first)),
      (Some("./other/lib.sh"), Some(&second)),
      (Some("./lib.sh"), Some(&first)),
      (Some(""), None),
      (None, Some(&first)),
    ] {
      let mut settings = app.settings().clone();
      if let Some(value) = value {
        settings
          .initial
          .environment
          .insert(b"CONFIG".to_vec(), value.as_bytes().to_vec());
      } else {
        settings.initial.environment.remove(b"CONFIG".as_slice());
      }
      app.settings = settings;
      app.invalidate_inputs();
      prepare(&mut app, &cancel).unwrap();
      for path in [&first, &second] {
        assert_eq!(
          app.document_id(path).is_some(),
          target == Some(path),
          "{value:?}"
        );
      }
    }
  }
}

mod paths {
  use super::fixtures;
  use super::{dependencies, prepare};
  use crate::interface::application::engine::{
    DependencyState, InitialEnvironment,
  };
  use crate::interface::application::{Application, Settings};
  use fixtures::Fixture;
  use std::sync::atomic::AtomicBool;

  #[test]
  fn glob_sources_respect_hidden_entries_and_directory_boundaries() {
    let fixture = Fixture::new();
    fixture.write("lib/visible.sh", "visible=1");
    fixture.write("lib/.hidden.sh", "hidden=1");
    fixture.write("lib/nested/leaf.sh", "nested=1");
    fixture.write("dot/x.hidden.sh", "visible=1");
    fixture.write("dot/.hidden.sh", "hidden=1");
    fixture.write("escaped/\\x.sh", "escaped=1");
    for (source, targets) in [
      (". ./lib/*.sh", vec!["lib/visible.sh"]),
      (". ./lib/.*.sh", vec!["lib/.hidden.sh"]),
      (". ./lib/*/leaf.sh", vec!["lib/nested/leaf.sh"]),
      (". ./lib/visible.sh/*.sh", vec![]),
      (". ./lib/missing*.sh", vec![]),
      (". './lib/*.sh'", vec![]),
      (". ./dot/*[.]hidden.sh", vec!["dot/x.hidden.sh"]),
      (
        r"pattern='./lib/\v*.sh'; . $pattern",
        vec!["lib/visible.sh"],
      ),
      (r"pattern='./\lib/*.sh'; . $pattern", vec!["lib/visible.sh"]),
      (r"pattern='./escaped/\*.sh'; . $pattern", vec![]),
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      prepare(&mut app, &cancelled).unwrap();
      let id = app.document_id(&main).unwrap();
      let actual: Vec<_> = app
        .document_links(id, &cancelled)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect();
      let expected: Vec<_> = targets
        .into_iter()
        .map(|path| fixture.root.join(path))
        .collect();
      assert_eq!(actual, expected, "{source}");
    }
  }

  #[test]
  fn computed_words_and_globs_resolve_sources_from_the_current_directory() {
    let fixture = Fixture::new();
    let library = fixture.write("sub/library2.sh", "found=1\n");
    for source in [
      "cd ./sub; . ./library2.sh",
      "eval 'cd /'; . ./sub/library2.sh",
      "PATH=./sub; eval 'PATH=/other; cd /'; . library2.sh",
      "alias .='echo'; . ./sub/library2.sh",
      "n=1; . ./sub/library$(( $n + 1 )).sh",
      "base=./sub/file; . \"${base%/*}/library$((1+1)).sh\"",
      "set -- ./sub/library2.sh; . \"$1\"",
      "read tail; set -- ./sub/library2.sh \"$tail\"; . \"$@\"",
      "set -- ./sub/library2.sh; for lib; do . \"$lib\"; done",
      "f() { . \"$1\"; }; f ./sub/library2.sh",
      "HOME=./sub; . ~/library2.sh",
      "cd ./sub; . ./library[0-9].sh",
      "pattern='./sub/*.sh'; . $pattern",
      "set -- ./sub/*.sh; . \"$1\"",
      "CDPATH=.; cd sub; . ./library2.sh",
      "HOME=./sub; cd; . ./library2.sh",
      "cd sub; cd ..; cd -; . ./library2.sh",
      "cd sub/; . ./library2.sh",
      "cd sub; (cd ..); . ./library2.sh",
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      assert!(
        app.document_id(&library).is_some(),
        "{source}: {:?}",
        app.failures()
      );
      assert_eq!(
        app
          .workspace()
          .symbols()
          .filter(|symbol| symbol.name == b"found")
          .count(),
        1,
        "{source}"
      );
    }
  }

  #[test]
  fn cd_simplifies_directory_spelling_for_pwd_and_oldpwd() {
    use crate::interface::application::engine::{
      OccurrenceKind, Value, ValueIssue,
    };
    let fixture = Fixture::new();
    fixture.write("sub/a/file.sh", "");
    let root = fixture.root.to_str().unwrap();
    let exact = |path: String| Value::Literal(path.into_bytes());
    let sub = exact(format!("{root}/sub"));
    let sub_a = exact(format!("{root}/sub/a"));
    for (source, expected) in [
      ("cd sub; result=$PWD".to_owned(), sub.clone()),
      ("cd ./sub/./a; result=$PWD".to_owned(), sub_a.clone()),
      ("CDPATH=sub/; cd a; result=$PWD".to_owned(), sub_a.clone()),
      (
        format!("cd /{root}/sub; result=$PWD"),
        exact(format!("/{root}/sub")),
      ),
      ("cd /..; result=$PWD".to_owned(), exact("/..".to_owned())),
      ("cd sub; cd ..; result=$OLDPWD".to_owned(), sub.clone()),
      ("cd sub; result=$(command -p pwd)".to_owned(), sub.clone()),
      ("cd sub/; result=$PWD".to_owned(), sub.clone()),
      (
        "cd sub; cd ..; result=$PWD".to_owned(),
        exact(root.to_owned()),
      ),
      ("cd sub; cd .; result=$PWD".to_owned(), sub.clone()),
      ("cd sub//a; result=$PWD".to_owned(), sub_a),
      ("cd sub/; cd a; result=$OLDPWD".to_owned(), sub.clone()),
      ("cd sub/; result=$(command -p pwd)".to_owned(), sub),
      (
        "cd /..; result=$(command -p pwd)".to_owned(),
        Value::Unknown(ValueIssue::UnsupportedExpansion),
      ),
      (
        "readonly PWD; PWD=/x; result=$(command -p pwd)".to_owned(),
        exact(root.to_owned()),
      ),
      (
        "readonly PWD; cd sub; result=$(command -p pwd)".to_owned(),
        Value::Unknown(ValueIssue::Dynamic),
      ),
    ] {
      let source = format!("{source}; : \"$result\"");
      let main = fixture.write("main.sh", &source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      app.classify(id, &AtomicBool::new(false)).unwrap();
      let read = app
        .workspace()
        .document(id)
        .unwrap()
        .occurrences()
        .iter()
        .rev()
        .find(|site| {
          site.kind == OccurrenceKind::VariableExpansion
            && site.name == Value::Literal(b"result".to_vec())
        })
        .unwrap()
        .id;
      assert_eq!(
        super::values(app.workspace().analysis().unwrap(), read)
          .map(|(_, value)| value.clone())
          .collect::<Vec<_>>(),
        [expected],
        "{source}"
      );
    }
  }

  #[test]
  fn absolute_cd_recovers_the_directory_after_an_unknown_cd() {
    let fixture = Fixture::new();
    let library = fixture.write("sub/library.sh", "found=1");
    let directory = library.parent().unwrap().to_str().unwrap();
    for (source, resolved) in [
      (
        format!(
          "read target; cd \"$target\"; cd '{directory}'; . ./library.sh"
        ),
        true,
      ),
      (
        format!(
          "trap \"cd '{directory}'; . ./library.sh\" EXIT; read target; cd \"$target\""
        ),
        true,
      ),
      (
        format!("trap \"cd '{directory}'; . ./library.sh\" EXIT; cd ./sub"),
        true,
      ),
      (
        format!(
          "trap \"cd '{directory}'; . ./library.sh\" EXIT; PWD=/elsewhere"
        ),
        false,
      ),
      (
        format!("read name; read \"$name\"; cd '{directory}'; . ./library.sh"),
        false,
      ),
      (
        format!("read name; read \"x$name\"; cd '{directory}'; . ./library.sh"),
        true,
      ),
      (
        format!(
          "f() {{ cd \"$1\"; f x; }}; f a; cd '{directory}'; . ./library.sh"
        ),
        true,
      ),
    ] {
      let main = fixture.write("main.sh", &source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      assert_eq!(app.document_id(&library).is_some(), resolved, "{source}");
    }
  }

  #[test]
  fn filesystem_queries_are_refreshed_and_dynamic_paths_remain_unresolved() {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", ". ./lib/*.sh\n");
    let a = fixture.write("lib/a.sh", "a=1\n");
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.add_input(&main);
    let cancel = AtomicBool::new(false);
    prepare(&mut app, &cancel).unwrap();
    assert!(app.document_id(&a).is_some());
    std::fs::remove_file(&a).unwrap();
    let b = fixture.write("lib/b.sh", "b=1\n");
    prepare(&mut app, &cancel).unwrap();
    assert!(app.document_id(&a).is_none());
    assert!(app.document_id(&b).is_some());
    super::overlay(
      &mut app,
      &main,
      Some(
        "if choice; then d=./lib; else d=./elsewhere; fi; cd \"$d\"; . ./b.sh",
      ),
      &cancel,
    )
    .unwrap();
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    assert!(matches!(
      dependencies(&app, id).next().unwrap().1,
      DependencyState::Unresolved(_)
    ));
  }

  #[cfg(unix)]
  #[test]
  fn logical_and_physical_cd_use_different_parents_of_a_symbolic_link() {
    let fixture = Fixture::new();
    let logical = fixture.write("library.sh", "logical=1\n");
    let physical = fixture.write("real/library.sh", "physical=1\n");
    std::fs::create_dir(fixture.root.join("real/sub")).unwrap();
    std::os::unix::fs::symlink("real/sub", fixture.root.join("link")).unwrap();
    for (option, expected) in [("-L", &logical), ("-P", &physical)] {
      let main = fixture
        .write("main.sh", &format!("cd {option} link/..; . ./library.sh"));
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      assert!(app.document_id(expected).is_some(), "{option}");
      assert_eq!(app.workspace().documents().count(), 2);
    }
  }
}

mod session {
  use super::fixtures;
  use super::prepare;

  use crate::interface::application::{Error, Settings};
  use fixtures::Fixture;

  #[test]
  fn position_queries_keep_the_original_text_after_rejected_reload_and_retry() {
    use crate::interface::application::Application;
    use std::sync::atomic::AtomicBool;
    for operation in ["definition", "references", "prepare", "rename"] {
      let fixture = Fixture::new();
      let original = "first=1; : $first\n";
      let path = fixture.write("main.sh", original);
      let mut app =
        Application::new(Settings::new(fixture.root.clone())).unwrap();
      app.add_input(&fixture.root);
      let cancelled = AtomicBool::new(false);
      prepare(&mut app, &cancelled).unwrap();
      let query = |app: &mut Application, offset| match operation {
        "definition" => app
          .definitions(app.document_id(&path).unwrap(), offset, &cancelled)
          .map(|sites| sites.len()),
        "references" => app
          .references(&path, offset, true, &cancelled)
          .map(|sites| sites.len()),
        "prepare" => app
          .prepare_rename(&path, offset, &cancelled)
          .map(|target| usize::from(target.is_some())),
        _ => app
          .rename(&path, offset, "renamed", &cancelled)
          .map(|edits| edits.len()),
      };
      assert!(query(&mut app, 0).unwrap() > 0, "{operation}");
      fixture.write("main.sh", "other=2; first=1; : $first\n");
      for _ in 0..2 {
        assert!(
          matches!(query(&mut app, 0), Err(Error::Superseded)),
          "{operation}"
        );
        assert_eq!(
          app
            .workspace()
            .document(app.document_id(&path).unwrap())
            .unwrap()
            .source(),
          original,
          "{operation}"
        );
      }
      prepare(&mut app, &cancelled).unwrap();
      assert!(query(&mut app, 9).unwrap() > 0, "{operation}");
    }
  }
}

mod sources {
  use super::fixtures;
  use super::{dependencies, prepare};
  use crate::interface::application::engine::{
    DependencyState, InitialEnvironment, OccurrenceKind, ResolutionIssue, Value,
  };
  use crate::interface::application::{Application, Settings};
  use fixtures::Fixture;
  use std::sync::atomic::AtomicBool;

  fn application(fixture: &Fixture) -> Application {
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    Application::new(settings).unwrap()
  }

  #[test]
  fn source_annotations_share_links_definitions_tokens_and_rename_without_inventing_values()
   {
    use crate::interface::application::engine::{Modifier, SemanticKind};
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "readonly value=ok\nf() { :; }\n");
    let source = "read path\n# shellsmith source=\"./lib.sh\"\n. \"$path\"\nf\n: \"$value\" \"$path\"\n";
    let main = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    app.add_input(&main);
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].1, library);
    assert_eq!(&source[links[0].0.bytes.clone()], "\"$path\"");
    let call = source.find("\nf\n").unwrap() + 1;
    let definitions = app.definitions(id, call, &cancel).unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(
      app.path(definitions[0].snapshot.document),
      Some(library.as_path())
    );
    assert_eq!(definitions[0].bytes, 18..19);
    let spans = app.classify(id, &cancel).unwrap();
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (call..call + 1)
          && span.kind == SemanticKind::Function)
    );
    let value = source.rfind("$value").unwrap() + 1;
    assert!(
      spans
        .iter()
        .any(|span| span.location.bytes == (value..value + 5)
          && span.modifiers.contains(&Modifier::Readonly))
    );
    let analysis = app.workspace().analysis().unwrap();
    for site in app
      .workspace()
      .document(id)
      .unwrap()
      .occurrences()
      .iter()
      .filter(|site| {
        site.kind == OccurrenceKind::VariableExpansion
          && site.name == Value::Literal(b"path".to_vec())
      })
    {
      assert!(
        super::values(analysis, site.id)
          .all(|(_, value)| matches!(value, Value::Unknown(_)))
      );
    }
    let edits = app.rename(&main, call, "renamed", &cancel).unwrap();
    assert_eq!(edits.len(), 2);
    let mut edited_main = source.to_owned();
    let mut edited_library = "readonly value=ok\nf() { :; }\n".to_owned();
    for edit in edits.iter().rev() {
      let text =
        if app.path(edit.location.snapshot.document) == Some(main.as_path()) {
          &mut edited_main
        } else {
          &mut edited_library
        };
      text.replace_range(edit.location.bytes.clone(), &edit.new_text);
    }
    assert_eq!(edited_main, source.replace("\nf\n", "\nrenamed\n"));
    assert_eq!(edited_library, "readonly value=ok\nrenamed() { :; }\n");
  }

  #[test]
  fn source_annotations_follow_nested_and_deferred_bodies_and_refresh_with_overlays()
   {
    let fixture = Fixture::new();
    let source =
      "read path\n# shellsmith source=./nested/first.sh\n. \"$path\"\n";
    let main = fixture.write("main.sh", source);
    let first_source =
      "read path\n# shellsmith source=./second.sh\n. \"$path\"\n";
    let first = fixture.write("nested/first.sh", first_source);
    let second = fixture.write("nested/second.sh", "f() { :; }\n");
    let third = fixture.write("nested/third.sh", "g() { :; }\n");
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    app.add_input(&main);
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&first).unwrap();
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"f")
        .count(),
      1
    );
    assert_eq!(
      app
        .document_links(id, &cancel)
        .unwrap()
        .iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [&second]
    );
    super::overlay(
      &mut app,
      &first,
      Some("read path\n# shellsmith source=./third.sh\n. \"$path\"\n"),
      &cancel,
    )
    .unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"f")
        .count(),
      0
    );
    assert_eq!(
      app
        .workspace()
        .symbols()
        .filter(|symbol| symbol.name == b"g")
        .count(),
      1
    );
    let id = app.document_id(&first).unwrap();
    assert_eq!(
      app
        .document_links(id, &cancel)
        .unwrap()
        .iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>(),
      [&third]
    );
    for source in [
      "read path\nf() {\n# shellsmith source=./nested/second.sh\n. \"$path\"\n}\nf\n",
      "f() {\n# shellsmith source=./nested/second.sh\n. \"$path\"\n}\n",
      "read path\ntrap '# shellsmith source=./nested/second.sh\n. \"$path\"' EXIT\n",
    ] {
      let mut app = application(&fixture);
      super::overlay(&mut app, &main, Some(source), &cancel).unwrap();
      let id = app.document_id(&main).unwrap();
      assert_eq!(
        app
          .document_links(id, &cancel)
          .unwrap()
          .iter()
          .map(|(_, path)| path)
          .collect::<Vec<_>>(),
        if source.starts_with("f()") {
          vec![]
        } else {
          vec![&second]
        },
        "{source}"
      );
    }
  }

  #[test]
  fn source_discovery_continues_after_termination_in_entries_and_sourced_files()
  {
    let fixture = Fixture::new();
    let first =
      fixture.write("first.sh", "set -u; exit; next=./second.sh; f() { :; }\n");
    let second = fixture.write("second.sh", "exec replacement; g() { :; }\n");
    for source in [
      "lib=./first.sh; set -eu; false; : \"$missing\"; exit; . \"$lib\"; . \"$next\"; f; g",
      "lib=./first.sh; set -o errexit -o nounset; exec replacement; . \"$lib\"; . \"$next\"; f; g",
    ] {
      let main = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      prepare(&mut app, &cancelled).unwrap();
      assert!(app.document_id(&first).is_some(), "{source}");
      assert!(app.document_id(&second).is_some(), "{source}");
      let id = app.document_id(&main).unwrap();
      assert!(
        dependencies(&app, id)
          .all(|(_, state)| { matches!(state, DependencyState::Resolved(_)) }),
        "{source}"
      );
      let spans = app.classify(id, &cancelled).unwrap();
      assert_eq!(
        spans
          .iter()
          .filter(|span| {
            span.kind
              == crate::interface::application::engine::SemanticKind::Function
          })
          .count(),
        2,
        "{source}"
      );
    }
  }

  #[test]
  fn assigned_source_paths_load_nested_files_and_recompute_after_changes() {
    let fixture = Fixture::new();
    let main = fixture.write(
      "main.sh",
      "base=./lib; file=$base/first.sh; . \"$file\"; . \"$next\"; f\n",
    );
    let first = fixture.write("lib/first.sh", "next=./lib/second.sh\n");
    let second = fixture.write("lib/second.sh", "f() { :; }\n");
    let third = fixture.write("lib/third.sh", "other() { :; }\n");
    let mut app = application(&fixture);
    app.add_input(&main);
    let cancelled = AtomicBool::new(false);
    prepare(&mut app, &cancelled).unwrap();
    assert!(app.document_id(&second).is_some());
    assert!(app.document_id(&third).is_none());
    let id = app.document_id(&main).unwrap();
    assert!(app.classify(id, &cancelled).unwrap().iter().any(|span| {
      span.kind == crate::interface::application::engine::SemanticKind::Function
    }));
    super::overlay(&mut app, &first, Some("next=./lib/third.sh\n"), &cancelled)
      .unwrap();
    prepare(&mut app, &cancelled).unwrap();
    assert!(app.document_id(&second).is_none());
    assert!(app.document_id(&third).is_some());
  }

  #[test]
  fn assigned_source_operands_use_pre_prefix_values_and_merge_call_contexts() {
    let fixture = Fixture::new();
    let first = fixture.write("first.sh", "first=1\n");
    let second = fixture.write("second.sh", "second=1\n");
    for (source, expected) in [
      (
        "lib=./first.sh; lib=./second.sh . \"$lib\"; . \"$lib\"",
        vec![first.clone(), second.clone()],
      ),
      (
        "if condition; then lib=./first.sh; else lib=./first.sh; fi; . \"$lib\"",
        vec![first.clone()],
      ),
      (
        "if condition; then lib=a/first.sh; else lib=b/first.sh; fi; . \"./${lib#*/}\"",
        vec![first.clone()],
      ),
      (
        "if condition; then base=fi; tail=rst.sh; else base=f; tail=irst.sh; fi; copied=$base; . \"./$copied$tail\"",
        vec![first.clone()],
      ),
      (
        "if condition; then lib=./first.sh; else lib=./second.sh; fi; . \"$lib\"",
        vec![],
      ),
      (
        "f() { . \"$lib\"; }; lib=./first.sh; f; lib=./second.sh; f",
        vec![],
      ),
      ("lib=./first.sh; . $lib", vec![first.clone()]),
      (
        "for lib in ./first.sh; do . \"$lib\"; done",
        vec![first.clone()],
      ),
      (
        "for lib in ./first.sh ./second.sh; do . \"$lib\"; done",
        vec![],
      ),
    ] {
      let main = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      app.add_input(&main);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      let actual: Vec<_> = dependencies(&app, id)
        .filter_map(|(_, state)| {
          if let DependencyState::Resolved(snapshot) = state {
            app
              .path(snapshot.document)
              .map(std::path::Path::to_path_buf)
          } else {
            None
          }
        })
        .collect();
      assert_eq!(actual, expected, "{source}");
    }
  }

  #[test]
  fn copied_environment_values_survive_overwrites_without_environment_modifiers()
   {
    let fixture = Fixture::new();
    let main = fixture.write(
      "main.sh",
      "base=$HOME; HOME=./elsewhere; . \"$base/lib.sh\"; : \"$HOME\"\n",
    );
    let library = fixture.write("lib.sh", "f() { :; }\n");
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    settings.initial.environment.insert(
      b"HOME".to_vec(),
      fixture.root.as_os_str().as_encoded_bytes().to_vec(),
    );
    let mut app = Application::new(settings).unwrap();
    app.add_input(&main);
    let cancelled = AtomicBool::new(false);
    prepare(&mut app, &cancelled).unwrap();
    assert!(app.document_id(&library).is_some());
    let id = app.document_id(&main).unwrap();
    let spans = app.classify(id, &cancelled).unwrap();
    let marked: Vec<_> = spans
      .iter()
      .filter(|span| {
        span.modifiers.contains(
          &crate::interface::application::engine::Modifier::Environment,
        )
      })
      .map(|span| span.location.bytes.clone())
      .collect();
    assert_eq!(marked, vec![6..10]);
  }

  #[test]
  fn limited_or_unsupported_assignment_values_never_become_resolved_sources() {
    let fixture = Fixture::new();
    fixture.write("lib.sh", "found=1\n");
    for (source, limit, expected) in [
      ("lib=./lib.sh; . \"$lib\"", 7, 0),
      ("lib=./lib.sh; . \"$lib\"", 8, 1),
      ("lib=$(printf ./lib.sh); . \"$lib\"", 8, 0),
      ("lib=./lib.sh; eval change; . \"$lib\"", 8, 1),
    ] {
      let main = fixture.write("main.sh", source);
      let mut app = application(&fixture);
      let mut settings = app.settings().clone();
      settings.limits.value_bytes = limit;
      app.settings = settings;
      app.invalidate_inputs();
      app.add_input(&main);
      let cancelled = AtomicBool::new(false);
      prepare(&mut app, &cancelled).unwrap();
      assert_eq!(
        app
          .workspace()
          .symbols()
          .filter(|symbol| symbol.name == b"found")
          .count(),
        expected,
        "{source} with {limit}"
      );
      if limit == 7 {
        let id = app.document_id(&main).unwrap();
        app.classify(id, &cancelled).unwrap();
        assert!(
          app
            .workspace()
            .analysis()
            .unwrap()
            .gaps()
            .iter()
            .any(|gap| gap.issue == ResolutionIssue::ValueLimit)
        );
      }
    }
  }

  #[test]
  fn source_invocations_use_expanded_names_operands_and_search_conditions() {
    let fixture = fixtures::Fixture::new();
    fixture.write("lib.sh", "loaded=yes\n");
    fixture.write("other/lib.sh", "loaded=other\n");
    for (source, expected) in [
      ("op=.; \"$op\" ./lib.sh; : \"$loaded\"", "yes"),
      ("command . ./lib.sh; : \"$loaded\"", "yes"),
      (". -- ./lib.sh; : \"$loaded\"", "yes"),
      ("p=./lib.sh; . $p; : \"$loaded\"", "yes"),
      (". \"${p:-./lib.sh}\"; : \"$loaded\"", "yes"),
      ("PATH=./other; . lib.sh; : \"$loaded\"", "other"),
      (
        "unused() { cd /unknown; }; . ./lib.sh; : \"$loaded\"",
        "yes",
      ),
      ("(cd /unknown); . ./lib.sh; : \"$loaded\"", "yes"),
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      app.classify(id, &AtomicBool::new(false)).unwrap();
      let analysis = app.workspace().analysis().unwrap();
      let read = app
        .workspace()
        .document(id)
        .unwrap()
        .occurrences()
        .iter()
        .rev()
        .find(|site| {
          site.kind == OccurrenceKind::VariableExpansion
            && site.name == Value::Literal(b"loaded".to_vec())
        })
        .unwrap();
      assert_eq!(
        super::values(analysis, read.id)
          .map(|(_, value)| value.clone())
          .collect::<Vec<_>>(),
        vec![Value::Literal(expected.as_bytes().to_vec())],
        "{source}"
      );
      assert!(
        app
          .workspace()
          .symbols()
          .filter(|symbol| symbol.name == b"loaded")
          .any(|symbol| app.path(symbol.id.snapshot.document).is_some_and(
            |path| path.ends_with(if expected == "other" {
              "other/lib.sh"
            } else {
              "lib.sh"
            })
          )),
        "{source}"
      );
      let references: Vec<_> = dependencies(&app, id)
        .map(|(_, state)| state.clone())
        .collect();
      assert!(
        references.iter().any(|state| matches!(
          state,
          crate::interface::application::engine::DependencyState::Resolved(_)
        )),
        "{source}: {references:?}"
      );
    }
  }

  #[test]
  fn search_conditions_follow_execution_order_and_join_only_needed_inputs() {
    use crate::interface::application::engine::DependencyState;
    let fixture = fixtures::Fixture::new();
    let first = fixture.write("first/lib.sh", "loaded=first\n");
    let second = fixture.write("second/lib.sh", "loaded=second\n");
    let current = fixture.write("lib.sh", "loaded=current\n");
    let percent = fixture.write("percent%/lib.sh", "loaded=percent\n");
    for (source, expected) in [
      (
        "PATH=./first; . lib.sh; PATH=./second; . lib.sh",
        vec![first.clone(), second.clone()],
      ),
      ("PATH=:./first; . lib.sh", vec![current.clone()]),
      ("PATH=; . lib.sh", vec![current.clone()]),
      ("PATH=./percent%; . lib.sh", vec![percent.clone()]),
      (
        "PATH=./first; PATH=./second x=$(. lib.sh) command :",
        vec![second.clone()],
      ),
      ("PATH=./first; PATH=./second . lib.sh", vec![second.clone()]),
      (
        "PATH=./first; name=lib.sh; PATH=./second . \"$name\"",
        vec![second.clone()],
      ),
      (
        "PATH=./first; (PATH=./second); . lib.sh",
        vec![first.clone()],
      ),
      (
        "f() { PATH=./second; }; PATH=./first; f; . lib.sh",
        vec![second.clone()],
      ),
      (
        "if condition; then PATH=./first; else PATH=./second; fi; . lib.sh",
        vec![],
      ),
      (
        "if condition; then PATH=./first; else PATH=./first; fi; . lib.sh",
        vec![first.clone()],
      ),
      (". ./missing; . ./first/lib.sh", vec![]),
      ("command command -- . ./first/lib.sh", vec![first.clone()]),
    ] {
      let main = fixture.write("main.sh", source);
      let mut settings = Settings::new(fixture.root.clone());
      settings.initial = InitialEnvironment::empty();
      let mut app = Application::new(settings).unwrap();
      app.add_input(&main);
      prepare(&mut app, &AtomicBool::new(false)).unwrap();
      let id = app.document_id(&main).unwrap();
      let actual: Vec<_> = dependencies(&app, id)
        .filter_map(|(_, state)| match state {
          DependencyState::Resolved(snapshot) => {
            Some(app.path(snapshot.document).unwrap().to_path_buf())
          }
          _ => None,
        })
        .collect();
      assert_eq!(actual, expected, "{source}");
    }
  }

  #[cfg(unix)]
  #[test]
  fn source_path_search_keeps_unreadable_files_and_refreshes_after_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let first = fixture.write("first/lib.sh", "first=1\n");
    let second = fixture.write("second/lib.sh", "second=2\n");
    std::fs::create_dir_all(fixture.root.join("directory/lib.sh")).unwrap();
    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o000))
      .unwrap();
    if std::fs::read(&first).is_ok() {
      // Permissions do not restrict this user.
      return;
    }
    let source = "PATH=./missing:./directory:./first:./second; . lib.sh\n";
    let main = fixture.write("main.sh", source);
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    super::overlay(&mut app, &main, Some(source), &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    for _ in 0..2 {
      assert_eq!(app.document_links(id, &cancel).unwrap(), []);
      assert_eq!(app.failures().len(), 1);
      assert_eq!(app.failures()[0].path, first);
      assert!(app.document_id(&second).is_none());
    }

    super::overlay(&mut app, &first, Some("live=1\n"), &cancel).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].1, first);
    assert_eq!(app.failures(), []);
    assert!(app.document_id(&second).is_none());
    super::overlay(&mut app, &first, None, &cancel).unwrap();
    assert!(app.failures().iter().any(|failure| failure.path == first));
    assert_eq!(app.document_links(id, &cancel).unwrap(), []);
    assert_eq!(app.failures().len(), 1);
    assert_eq!(app.failures()[0].path, first);

    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o644))
      .unwrap();
    prepare(&mut app, &cancel).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].1, first);
    assert_eq!(app.failures(), []);
    assert!(app.document_id(&second).is_none());

    std::fs::remove_file(&first).unwrap();
    prepare(&mut app, &cancel).unwrap();
    let links = app.document_links(id, &cancel).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].1, second);
    assert_eq!(app.failures(), []);
  }

  #[test]
  fn source_following_shares_trap_links_without_changing_the_callers_dependencies()
   {
    let fixture = Fixture::new();
    let library = fixture.write("lib.sh", "target=./first.sh\n");
    let first = fixture.write("first.sh", ":\n");
    let second = fixture.write("second.sh", ":\n");
    let hidden = fixture.write("hidden.sh", "hidden() { :; }\n");
    let source = ". ./lib.sh\ntrap 'target=./hidden.sh; . ./hidden.sh' INT EXIT\n. \"$target\"\n";
    let main = fixture.write("main.sh", source);
    let mut application =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    application.add_input(&main);
    let cancelled = AtomicBool::new(false);
    for expected in [first, second] {
      prepare(&mut application, &cancelled).unwrap();
      let id = application.document_id(&main).unwrap();
      let links = application.document_links(id, &cancelled).unwrap();
      assert_eq!(
        links.iter().map(|(_, target)| target).collect::<Vec<_>>(),
        vec![&library, &hidden, &expected]
      );
      assert!(application.document_id(&hidden).is_some());
      assert_eq!(application.workspace().documents().count(), 5);
      assert_eq!(
        application
          .workspace()
          .symbols()
          .filter(|symbol| symbol.name == b"hidden")
          .count(),
        1
      );
      fixture.write("lib.sh", "target=./second.sh\n");
    }
  }
}

mod definitions {
  use super::fixtures::Fixture;
  use super::prepare;
  use crate::interface::application::engine::InitialEnvironment;
  use crate::interface::application::{Application, Settings};
  use std::sync::atomic::AtomicBool;

  #[test]
  fn definitions_keep_complete_source_candidates_and_ignore_unrelated_files() {
    let fixture = Fixture::new();
    let source = "if condition; then . ./b.sh; else . ./a.sh; fi\nf\n";
    let main = fixture.write("main.sh", source);
    let first = fixture.write("a.sh", "f() { :; }\n");
    let second = fixture.write("b.sh", "f() { : 2; }\n");
    fixture.write("unrelated.sh", "f() { : 3; }\n");
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    app.add_input(&fixture.root);
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let offset = source.rfind("f\n").unwrap();
    let definitions = app.definitions(id, offset, &cancel).unwrap();
    assert_eq!(
      definitions
        .iter()
        .map(|location| {
          (
            app.path(location.snapshot.document).unwrap(),
            location.bytes.clone(),
          )
        })
        .collect::<Vec<_>>(),
      [(first.as_path(), 0..1), (second.as_path(), 0..1)]
    );
    super::overlay(&mut app, &first, Some("g() { :; }\n"), &cancel).unwrap();
    assert_eq!(app.definitions(id, offset, &cancel).unwrap(), []);
  }

  #[test]
  fn definitions_use_source_overlays_and_invalidate_changed_or_unavailable_targets()
   {
    let fixture = Fixture::new();
    let main_source = ". ./lib.sh\nf\n";
    let main = fixture.write("main.sh", main_source);
    let library = fixture.write("lib.sh", "f() { :; }\n");
    let cancel = AtomicBool::new(false);
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    let mut app = Application::new(settings).unwrap();
    super::overlay(&mut app, &main, Some(main_source), &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    let first = app.definitions(id, 11, &cancel).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(
      app.path(first[0].snapshot.document),
      Some(library.as_path())
    );
    assert_eq!(first[0].bytes, 0..1);
    super::overlay(
      &mut app,
      &library,
      Some("# unsaved\nf() { :; }\n"),
      &cancel,
    )
    .unwrap();
    let updated = app.definitions(id, 11, &cancel).unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].bytes, 10..11);
    assert_ne!(updated[0].snapshot, first[0].snapshot);
    super::overlay(&mut app, &library, Some("g() { :; }\n"), &cancel).unwrap();
    assert_eq!(app.definitions(id, 11, &cancel).unwrap(), []);
    super::overlay(&mut app, &library, None, &cancel).unwrap();
    assert_eq!(app.definitions(id, 11, &cancel).unwrap()[0].bytes, 0..1);
    std::fs::remove_file(&library).unwrap();
    prepare(&mut app, &cancel).unwrap();
    assert_eq!(app.definitions(id, 11, &cancel).unwrap(), []);
  }
}

mod references {
  use super::fixtures::Fixture;
  use super::prepare;
  use crate::interface::application::{Application, Settings};
  use std::sync::atomic::AtomicBool;

  #[test]
  fn classification_discards_stale_workspace_results_without_rejecting_preparation_changes()
   {
    let fixture = Fixture::new();
    let main = fixture.write("main.sh", "f() { :; }; f\n");
    fixture.write("other.sh", "x=1\n");
    let mut app =
      Application::new(Settings::new(fixture.root.clone())).unwrap();
    app.add_input(&fixture.root);
    let cancel = AtomicBool::new(false);
    prepare(&mut app, &cancel).unwrap();
    let id = app.document_id(&main).unwrap();
    app.definitions(id, 0, &cancel).unwrap();
    let tokens = app.classify(id, &cancel).unwrap();
    fixture.write("other.sh", "x=2\n");
    assert_eq!(app.classify(id, &cancel).unwrap(), tokens);
  }
}

mod rename {
  use super::fixtures::Fixture;
  use super::prepare;
  use crate::interface::application::engine::InitialEnvironment;
  use crate::interface::application::{Application, Error, Settings};
  use std::sync::atomic::AtomicBool;

  fn application(fixture: &Fixture) -> Application {
    let mut settings = Settings::new(fixture.root.clone());
    settings.initial = InitialEnvironment::empty();
    Application::new(settings).unwrap()
  }

  #[test]
  fn rename_rechecks_closed_inputs_and_uses_current_open_text() {
    let fixture = Fixture::new();
    let path = fixture.write("main.sh", "x=1; : $x");
    let cancel = AtomicBool::new(false);
    let mut app = application(&fixture);
    app.prepare_rename(&path, 0, &cancel).unwrap();
    fixture.write("main.sh", "x=1; y=2; : $x");
    assert!(matches!(
      app.rename(&path, 0, "y", &cancel),
      Err(Error::Superseded)
    ));
    assert!(matches!(
      app.rename(&path, 0, "y", &cancel),
      Err(Error::Superseded)
    ));
    prepare(&mut app, &cancel).unwrap();
    assert!(matches!(
      app.rename(&path, 0, "y", &cancel),
      Err(Error::RenameRefused(_))
    ));
    super::overlay(&mut app, &path, Some("x=1; : $x"), &cancel).unwrap();
    assert_eq!(app.rename(&path, 0, "y", &cancel).unwrap().len(), 2);
    super::overlay(&mut app, &path, Some("x=1; y=2; : $x"), &cancel).unwrap();
    assert!(matches!(
      app.rename(&path, 0, "y", &cancel),
      Err(Error::RenameRefused(_))
    ));
  }

  #[test]
  fn rename_requests_keep_trap_body_symbols_of_other_inputs() {
    let fixture = Fixture::new();
    let trap = fixture.write("a.sh", "trap 'f() { :; }' EXIT\n");
    let main = fixture.write("b.sh", "x=1; : $x\n");
    let mut app = application(&fixture);
    app.add_input(&fixture.root);
    let cancel = AtomicBool::new(false);
    prepare(&mut app, &cancel).unwrap();
    let symbols = |app: &Application| {
      let id = app.document_id(&trap).unwrap();
      let names: Vec<_> = app
        .document_symbols(id)
        .unwrap()
        .map(|entry| entry.details.symbol.name.clone())
        .collect();
      (names, app.workspace_symbols("f").count())
    };
    let expected = (vec![b"f".to_vec()], 1);
    assert_eq!(symbols(&app), expected);
    assert!(app.prepare_rename(&main, 0, &cancel).unwrap().is_some());
    assert_eq!(symbols(&app), expected);
    assert_eq!(app.rename(&main, 0, "y", &cancel).unwrap().len(), 2);
    assert_eq!(symbols(&app), expected);
  }
}

fn overlay(
  app: &mut Application,
  path: &Path,
  source: Option<&str>,
  cancelled: &AtomicBool,
) -> Result<(), Error> {
  let path = app.overlay_path(path, source.is_some())?;
  app.set_overlay(path.clone(), source.map(Arc::from));
  app.refresh_for(Some(&path), cancelled)
}

fn values(
  analysis: &engine::Analysis,
  occurrence: engine::OccurrenceId,
) -> impl Iterator<Item = (engine::ExecutionContextId, &engine::Value)> {
  analysis.contexts().iter().filter_map(move |context| {
    analysis
      .value(occurrence, context.id)
      .map(|value| (context.id, value))
  })
}
