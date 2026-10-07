//! The filter grammar of `GET /rest/<table>`: regular expressions, `isDistinctFrom`, any and
//! all of a list, `or` / `not` groups in the query string, array and JSON operators.
mod common;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use graphpg::catalog::CatalogInspector;
use graphpg::schema::SchemaBuilder;
use graphpg::server::{create_pool, create_router, DebugOptions, ServerState};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tower::ServiceExt;

struct Env {
    app: axum::Router,
    client: tokio_postgres::Client,
}

async fn env() -> Env {
    common::ensure_rest_extras_fixture().await;
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let catalog = CatalogInspector::new(&client)
        .inspect_schema("rest_extras")
        .await
        .unwrap();
    let graphql = SchemaBuilder::build(&catalog).unwrap();
    let state = ServerState::new(
        create_pool(common::db_url(), 4).unwrap(),
        graphql,
        catalog,
        "rest_extras".to_string(),
        DebugOptions::default(),
    );
    Env {
        app: create_router(Arc::new(state)),
        client,
    }
}

#[allow(dead_code)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("{}", self.text()))
    }
    #[allow(dead_code)]
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

impl Env {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let res = self
            .app
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }
}

/// The tests that write the same rows take turns.
static PEOPLE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn ids(env: &Env, uri: &str) -> Vec<i64> {
    let res = env.call("GET", uri, &[], vec![]).await;
    assert_eq!(res.status, StatusCode::OK, "{uri}: {}", res.text());
    let mut ids: Vec<i64> = res
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

async fn people(env: &Env) {
    env.client
        .batch_execute(
            "DELETE FROM rest_extras.people WHERE id BETWEEN 9401 AND 9406;
             INSERT INTO rest_extras.people VALUES
               (9401,'Ann'),(9402,'bob'),(9403,'Cy'),(9404,'Dee'),(9405,'ann'),(9406,'Eve');",
        )
        .await
        .unwrap();
}

const PEOPLE: &str = "filter[id][gte]=9401&filter[id][lte]=9406&fields=id";

#[tokio::test]
async fn regular_expressions_and_distinct_from() {
    let _turn = PEOPLE_LOCK.lock().await;
    let env = env().await;
    people(&env).await;
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[name][regex]=%5E%5BA-Z%5D")
        )
        .await,
        [9401, 9403, 9404, 9406]
    );
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[name][iregex]=%5Eann%24")
        )
        .await,
        [9401, 9405]
    );
    // PostgREST's names work too
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[name][imatch]=%5Eb")
        )
        .await,
        [9402]
    );
    // `isDistinctFrom` treats null as a value: rows whose `big` is not null
    let typed = ids(
        &env,
        "/rest/typed?fields=id&filter[big][isDistinctFrom]=null",
    )
    .await;
    assert!(typed.contains(&1) && !typed.contains(&2), "{typed:?}");
    let unlike = ids(
        &env,
        "/rest/typed?fields=id&filter[big][isDistinctFrom]=9007199254740993",
    )
    .await;
    assert!(unlike.contains(&2) && !unlike.contains(&1), "{unlike:?}");
    people_cleanup(&env).await;
}

async fn people_cleanup(env: &Env) {
    env.client
        .batch_execute("DELETE FROM rest_extras.people WHERE id BETWEEN 9401 AND 9406")
        .await
        .unwrap();
}

#[tokio::test]
async fn any_and_all_of_a_list() {
    let _turn = PEOPLE_LOCK.lock().await;
    let env = env().await;
    people(&env).await;
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[id][gtAll]=9402,9404")
        )
        .await,
        [9405, 9406]
    );
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[id][ltAny]=9401,9403")
        )
        .await,
        [9401, 9402]
    );
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[name][likeAny]=A%25,E%25")
        )
        .await,
        [9401, 9406]
    );
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[name][ilikeAll]=%25n%25,a%25")
        )
        .await,
        [9401, 9405]
    );
    people_cleanup(&env).await;
}

#[tokio::test]
async fn or_and_not_groups_in_the_query_string() {
    let _turn = PEOPLE_LOCK.lock().await;
    let env = env().await;
    people(&env).await;
    // groups next to plain conditions: all of them must hold
    assert_eq!(
        ids(
            &env,
            "/rest/people?fields=id&filter[id][gte]=9401&filter[id][lte]=9406\
             &filter[or][0][id][lt]=9402&filter[or][1][id][gt]=9405"
        )
        .await,
        [9401, 9406]
    );
    assert_eq!(
        ids(
            &env,
            &format!("/rest/people?{PEOPLE}&filter[not][name][eq]=Ann")
        )
        .await,
        [9402, 9403, 9404, 9405, 9406]
    );
    // a group of groups, and text cells stay text
    assert_eq!(
        ids(
            &env,
            "/rest/people?fields=id&filter[id][gte]=9401&filter[id][lte]=9406\
             &filter[or][0][name][eq]=Cy&filter[or][1][and][0][name][eq]=Eve&filter[or][1][and][1][id][gt]=1"
        )
        .await,
        [9403, 9406]
    );
    // the search body takes the same groups
    let res = env
        .call(
            "POST",
            "/rest/people/search",
            &[("content-type", "application/json")],
            br#"{"fields":["id"],"filter":{"id":{"gte":9401,"lte":9406},"or":[{"name":{"eq":"Cy"}},{"name":{"regex":"^E"}}]}}"#.to_vec(),
        )
        .await;
    assert_eq!(res.json().as_array().unwrap().len(), 2, "{}", res.text());
    people_cleanup(&env).await;
}

#[tokio::test]
async fn array_and_json_operators_in_the_query_string() {
    let env = env().await;
    let tags = |q: &str| format!("/rest/typed?fields=id&{q}");
    assert!(ids(&env, &tags("filter[tags][contains]=a"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[tags][contains]=a,b"))
        .await
        .contains(&1));
    assert!(!ids(&env, &tags("filter[tags][contains]=a,z"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[tags][overlaps]=z,b"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[tags][containedBy]=a,b,c"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[doc][hasKey]=a")).await.contains(&1));
    assert!(!ids(&env, &tags("filter[doc][hasKey]=zzz"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[doc][hasAnyKeys]=zzz,a"))
        .await
        .contains(&1));
    assert!(!ids(&env, &tags("filter[doc][hasAllKeys]=zzz,a"))
        .await
        .contains(&1));
    assert!(ids(
        &env,
        &tags("filter[doc][contains]=%7B%22a%22%3A%7B%22b%22%3A%5B1%5D%7D%7D")
    )
    .await
    .contains(&1));
    assert!(ids(&env, &tags("filter[doc][existsPath]=%24.a.b"))
        .await
        .contains(&1));

    // Range operators on span int4range: [1, 5)
    assert!(ids(&env, &tags("filter[span][contains]=3"))
        .await
        .contains(&1));
    assert!(!ids(&env, &tags("filter[span][contains]=10"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[span][strictlyLeftOf]=[10,20)"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[span][strictlyRightOf]=[0,1)"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[span][overlaps]=[4,10)"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[span][adjacentTo]=[5,10)"))
        .await
        .contains(&1));
    assert!(ids(&env, &tags("filter[span][containedBy]=[0,10)"))
        .await
        .contains(&1));

    // a bad value names the parameter
    let bad = env
        .call("GET", "/rest/people?filter[id][gtAny]=x", &[], vec![])
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let unknown = env
        .call("GET", "/rest/people?filter[id][regex]=x", &[], vec![])
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "regex is for text");
}
