use super::*;
use crate::engine::namespace::{NamespaceCollisionDetector, NamespaceQualification};

#[tokio::test]
async fn captured_backup_parent_and_absence_never_adopt_foreign_roots() {
    for pending in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let main = fixture.path().join("main");
        let parent = fixture.path().join("parent");
        std::fs::create_dir(&main).unwrap();
        std::fs::create_dir(&parent).unwrap();
        let backup = parent.join("backup");
        if !pending {
            std::fs::create_dir(&backup).unwrap();
        }
        std::fs::write(main.join("file"), b"original").unwrap();
        let main = RootedFs::open(main).await.unwrap();
        let source = RelativePath::new("file").unwrap();
        let expected = main.path_identity_blocking(&source).unwrap().unwrap().1;
        let root = BackupRoot::capture(backup.clone()).await;
        let (namespace, mut entries) = root.prepare().await.unwrap();
        entries.close().await.unwrap();
        let mut detector = NamespaceCollisionDetector::new();
        detector
            .record(&RelativePath::new("nested/file~").unwrap(), &namespace)
            .await
            .unwrap();
        detector.finish().await.unwrap();
        namespace.seal().await.unwrap();
        if pending {
            std::fs::create_dir(&backup).unwrap(); // foreign mkdir winner
        } else {
            std::fs::rename(&parent, fixture.path().join("held-parent")).unwrap();
            std::fs::create_dir_all(&backup).unwrap();
        }
        let result = main.backup_file_with_root_blocking(
            &source,
            &backup.join("nested/file~"),
            expected,
            Some(&root),
        );
        assert!(result.is_err());
        assert!(!backup.join("nested").exists());
        assert_eq!(
            std::fs::read(main.root_path().join("file")).unwrap(),
            b"original"
        );
    }
}
