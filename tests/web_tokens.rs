//! The access tokens page: tokens are shown once, stored as a hash, listed,
//! revoked, and expire.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::http::StatusCode;
use common::{TestApp, add_repo, page, post_page, shown_token, urlencode};
use kehribar::db::repos;
use kehribar::db::tokens::{self, TokenKind};
use kehribar::db::unix_now;
use kehribar::db::usage::DAY_SECONDS;
use kehribar::web::auth::hash_token;

async fn create(t: &TestApp, body: &str) -> (StatusCode, String) {
    post_page(t, "/settings/tokens", body).await
}

#[tokio::test]
async fn a_new_token_is_shown_once_and_stored_as_a_hash() {
    let t = TestApp::new().await;
    let (status, html) = page(&t, "/settings/tokens").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Henüz token yok."), "{html}");

    let name = urlencode("backup server");
    let (status, html) = create(&t, &format!("kind=clone&name={name}&scope=all")).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let token = shown_token(&html, "khc_");
    assert!(html.contains("Bir daha gösterilmeyecek"));

    let found = tokens::find_valid(
        &t.state.pool,
        &hash_token(&token),
        TokenKind::Clone,
        unix_now(),
    )
    .await
    .unwrap()
    .expect("the stored token");
    assert_eq!(found.name, "backup server");
    assert!(found.all_repos);
    assert_eq!(found.expires_at, None);
    // The token itself is stored nowhere.
    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM access_tokens")
        .fetch_all(&t.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, [hash_token(&token)]);
    // A clone token is not an API token.
    let as_api = tokens::find_valid(
        &t.state.pool,
        &hash_token(&token),
        TokenKind::Api,
        unix_now(),
    )
    .await
    .unwrap();
    assert_eq!(as_api, None);

    let (_, list) = page(&t, "/settings/tokens").await;
    assert!(list.contains("backup server"));
    assert!(list.contains("Tüm repolar"));
    assert!(!list.contains(&token));
}

#[tokio::test]
async fn a_clone_token_for_selected_repositories_keeps_them() {
    let t = TestApp::new().await;
    let one = add_repo(&t, "one").await;
    let two = add_repo(&t, "two").await;

    let (status, html) = create(&t, "kind=clone&name=ci&scope=selected").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("En az bir repo seçin"), "{html}");

    // An unknown repository id is dropped.
    let body = format!("kind=clone&name=ci&scope=selected&repo={one}&repo=999");
    let (status, html) = create(&t, &body).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let token = shown_token(&html, "khc_");
    let found = tokens::find_valid(
        &t.state.pool,
        &hash_token(&token),
        TokenKind::Clone,
        unix_now(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!found.all_repos);
    assert!(
        tokens::allows_repo(&t.state.pool, &found, one)
            .await
            .unwrap()
    );
    assert!(
        !tokens::allows_repo(&t.state.pool, &found, two)
            .await
            .unwrap()
    );
    let (_, list) = page(&t, "/settings/tokens").await;
    assert!(list.contains("example.com/team/one"), "{list}");

    // Deleting the repository takes it out of the token.
    repos::delete(&t.state.pool, one).await.unwrap();
    assert!(tokens::repo_names(&t.state.pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn api_tokens_have_a_permission_and_an_expiry() {
    let t = TestApp::new().await;
    let (status, html) = create(&t, "kind=api&name=script&permission=write&expiry=30").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    let token = shown_token(&html, "kha_");
    assert!(html.contains("Authorization: Bearer"));
    let hash = hash_token(&token);
    let now = unix_now();
    let found = tokens::find_valid(&t.state.pool, &hash, TokenKind::Api, now)
        .await
        .unwrap()
        .unwrap();
    assert!(found.can_write);
    let expires = found.expires_at.unwrap();
    assert!((expires - now - 30 * DAY_SECONDS).abs() < 60);
    // After its expiry the token is not valid.
    let later = tokens::find_valid(&t.state.pool, &hash, TokenKind::Api, expires)
        .await
        .unwrap();
    assert_eq!(later, None);

    for bad in [
        "kind=api&name=x&expiry=7",
        "kind=api&name=+++",
        "kind=other&name=x",
    ] {
        let (status, _) = create(&t, bad).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let long = "x".repeat(101);
    let (status, _) = create(&t, &format!("kind=api&name={long}")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_revoked_token_is_gone() {
    let t = TestApp::new().await;
    let (_, html) = create(&t, "kind=api&name=old&permission=read").await;
    let token = shown_token(&html, "kha_");
    let id: i64 = sqlx::query_scalar("SELECT id FROM access_tokens")
        .fetch_one(&t.state.pool)
        .await
        .unwrap();

    let response = t.post(&format!("/settings/tokens/{id}/revoke"), "").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let found = tokens::find_valid(
        &t.state.pool,
        &hash_token(&token),
        TokenKind::Api,
        unix_now(),
    )
    .await
    .unwrap();
    assert_eq!(found, None);
    let (_, list) = page(&t, "/settings/tokens?notice=revoked").await;
    assert!(list.contains("Token iptal edildi."));
    assert!(!list.contains(">old<"));

    let again = t.post(&format!("/settings/tokens/{id}/revoke"), "").await;
    assert_eq!(again.status(), StatusCode::NOT_FOUND);
    // The form needs the CSRF token.
    let request = common::form_request("/settings/tokens", "kind=api&name=x");
    let response = t.send(request).await;
    assert_ne!(response.status(), StatusCode::OK);
}
