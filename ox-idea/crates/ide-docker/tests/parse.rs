//! Parsing of recorded `docker … --format '{{json .}}'` output.

use ide_docker::{parse_containers, parse_images, parse_networks, parse_volumes};

const PS: &str = include_str!("fixtures/ps.jsonl");
const IMAGES: &str = include_str!("fixtures/images.jsonl");
const VOLUMES: &str = include_str!("fixtures/volumes.jsonl");
const NETWORKS: &str = include_str!("fixtures/networks.jsonl");

#[test]
fn containers_with_compose_labels() {
    let c = parse_containers(PS);
    assert_eq!(c.len(), 2, "non-JSON lines are skipped");
    let db = &c[0];
    assert_eq!(db.name, "shop-db-1");
    assert_eq!(db.state, "running");
    assert!(db.running());
    assert_eq!(db.status, "Up 2 hours (healthy)");
    assert_eq!(db.command, "docker-entrypoint.sh postgres");
    assert_eq!(db.compose_project.as_deref(), Some("shop"));
    assert_eq!(db.compose_service.as_deref(), Some("db"));
    assert_eq!(db.compose_working_dir.as_deref(), Some("/work/shop"));
    assert_eq!(db.compose_files, ["/work/shop/compose.yaml", "/work/shop/compose.override.yaml"]);
    assert_eq!(db.ports, "0.0.0.0:5432->5432/tcp");

    let web = &c[1];
    assert_eq!(web.name, "web", "leading slash of legacy names is dropped");
    assert!(!web.running());
    assert_eq!(web.compose_project, None);
}

#[test]
fn images_sorted_with_dangling_last() {
    let i = parse_images(IMAGES);
    let refs: Vec<String> = i.iter().map(|i| i.reference()).collect();
    assert_eq!(refs, ["nginx:1.27", "postgres:16", "aaaaaaaaaaaa"]);
    assert_eq!(i[1].size, "431MB");
    assert_eq!(i[1].created_since, "7 weeks ago");
}

#[test]
fn volumes_and_networks() {
    let v = parse_volumes(VOLUMES);
    assert_eq!(v.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["0a1b", "shop_pgdata"]);
    assert_eq!(v[1].compose_project.as_deref(), Some("shop"));
    assert_eq!(v[1].mountpoint, "/var/lib/docker/volumes/shop_pgdata/_data");

    let n = parse_networks(NETWORKS);
    assert_eq!(n.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["bridge", "host", "shop_default"]);
    assert_eq!(n[2].compose_project.as_deref(), Some("shop"));
    assert_eq!(n[1].driver, "host");
}
