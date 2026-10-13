//! Fixtures use the real source producers, never manufacture catalogue records.
use super::*;
use futures::TryStreamExt;

pub(crate) async fn scan(rooted: &RootedFs) {
    rooted
        .source_entry_stream(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
}

pub(crate) fn scan_blocking(rooted: &RootedFs) {
    let rooted = rooted.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(scan(&rooted));
    })
    .join()
    .unwrap();
}

pub(crate) fn selected_blocking(rooted: &RootedFs, path: RelativePath) {
    let rooted = rooted.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let entries: Vec<_> = rooted
                    .selected_source_entry_stream(path, Default::default())
                    .try_collect()
                    .await
                    .unwrap();
                assert_eq!(entries.len(), 1);
            });
    })
    .join()
    .unwrap();
}
