use chrono::NaiveDateTime;
use conduit::{Request, Response};
use conduit_router::RequestParams;
use diesel::*;

use Crate;
use db::RequestTransaction;
use schema::*;
use util::{CargoResult, RequestUtils};

#[derive(Clone, Identifiable, Queryable, QueryableByName, Debug)]
#[table_name = "categories"]
pub struct Category {
    pub id: i32,
    pub category: String,
    pub slug: String,
    pub description: String,
    pub crates_cnt: i32,
    pub created_at: NaiveDateTime,
}

#[derive(Associations, Insertable, Identifiable, Debug, Clone, Copy)]
#[belongs_to(Category)]
#[belongs_to(Crate)]
#[table_name = "crates_categories"]
#[primary_key(crate_id, category_id)]
pub struct CrateCategory {
    crate_id: i32,
    category_id: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EncodableCategory {
    pub id: String,
    pub category: String,
    pub slug: String,
    pub description: String,
    #[serde(with = "::util::rfc3339")] pub created_at: NaiveDateTime,
    pub crates_cnt: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EncodableCategoryWithSubcategories {
    pub id: String,
    pub category: String,
    pub slug: String,
    pub description: String,
    #[serde(with = "::util::rfc3339")] pub created_at: NaiveDateTime,
    pub crates_cnt: i32,
    pub subcategories: Vec<EncodableCategory>,
}

impl Category {
    pub fn encodable(self) -> EncodableCategory {
        let Category {
            crates_cnt,
            category,
            slug,
            description,
            created_at,
            ..
        } = self;
        EncodableCategory {
            id: slug.clone(),
            slug: slug.clone(),
            description: description.clone(),
            created_at: created_at,
            crates_cnt: crates_cnt,
            category: category,
        }
    }

    pub fn update_crate<'a>(
        conn: &PgConnection,
        krate: &Crate,
        slugs: &[&'a str],
    ) -> QueryResult<Vec<&'a str>> {
        use diesel::dsl::any;

        conn.transaction(|| {
            let categories = categories::table
                .filter(categories::slug.eq(any(slugs)))
                .load::<Category>(conn)?;
            let invalid_categories = slugs
                .iter()
                .cloned()
                .filter(|s| !categories.iter().any(|c| c.slug == *s))
                .collect();
            let crate_categories = categories
                .iter()
                .map(|c| CrateCategory {
                    category_id: c.id,
                    crate_id: krate.id,
                })
                .collect::<Vec<_>>();

            delete(CrateCategory::belonging_to(krate)).execute(conn)?;
            insert_into(crates_categories::table)
                .values(&crate_categories)
                .execute(conn)?;
            Ok(invalid_categories)
        })
    }

    pub fn count_toplevel(conn: &PgConnection) -> QueryResult<i64> {
        use self::categories::dsl::*;

        categories
            .filter(category.not_like("%::%"))
            .count()
            .get_result(conn)
    }

    pub fn toplevel(
        conn: &PgConnection,
        sort: &str,
        limit: i64,
        offset: i64,
    ) -> QueryResult<Vec<Category>> {
        use diesel::select;
        use diesel::dsl::*;

        let sort_sql = match sort {
            "crates" => "ORDER BY crates_cnt DESC",
            _ => "ORDER BY category ASC",
        };

        // Collect all the top-level categories and sum up the crates_cnt of
        // the crates in all subcategories
        select(sql::<categories::SqlType>(&format!(
            "c.id, c.category, c.slug, c.description,
                sum(c2.crates_cnt)::int as crates_cnt, c.created_at
             FROM categories as c
             INNER JOIN categories c2 ON split_part(c2.slug, '::', 1) = c.slug
             WHERE split_part(c.slug, '::', 1) = c.slug
             GROUP BY c.id
             {} LIMIT {} OFFSET {}",
            sort_sql, limit, offset
        ))).load(conn)
    }

    pub fn subcategories(&self, conn: &PgConnection) -> QueryResult<Vec<Category>> {
        use diesel::sql_types::Text;

        sql_query(
            "SELECT c.id, c.category, c.slug, c.description, \
             COALESCE (( \
             SELECT sum(c2.crates_cnt)::int \
             FROM categories as c2 \
             WHERE c2.slug = c.slug \
             OR c2.slug LIKE c.slug || '::%' \
             ), 0) as crates_cnt, c.created_at \
             FROM categories as c \
             WHERE c.category ILIKE $1 || '::%' \
             AND c.category NOT ILIKE $1 || '::%::%'",
        ).bind::<Text, _>(&self.category)
            .load(conn)
    }
}

#[derive(Insertable, AsChangeset, Default, Debug)]
#[table_name = "categories"]
pub struct NewCategory<'a> {
    pub category: &'a str,
    pub slug: &'a str,
}

impl<'a> NewCategory<'a> {
    /// Inserts the category into the database, or updates an existing one.
    pub fn create_or_update(&self, conn: &PgConnection) -> QueryResult<Category> {
        use schema::categories::dsl::*;

        insert_into(categories)
            .values(self)
            .on_conflict(slug)
            .do_update()
            .set(self)
            .get_result(conn)
    }
}

/// Handles the `GET /categories` route.
pub fn index(req: &mut Request) -> CargoResult<Response> {
    let conn = req.db_conn()?;
    let (offset, limit) = req.pagination(10, 100)?;
    let query = req.query();
    let sort = query.get("sort").map_or("alpha", String::as_str);

    let categories = Category::toplevel(&conn, sort, limit, offset)?;
    let categories = categories.into_iter().map(Category::encodable).collect();

    // Query for the total count of categories
    let total = Category::count_toplevel(&conn)?;

    #[derive(Serialize)]
    struct R {
        categories: Vec<EncodableCategory>,
        meta: Meta,
    }
    #[derive(Serialize)]
    struct Meta {
        total: i64,
    }

    Ok(req.json(&R {
        categories: categories,
        meta: Meta { total: total },
    }))
}

/// Handles the `GET /categories/:category_id` route.
pub fn show(req: &mut Request) -> CargoResult<Response> {
    let slug = &req.params()["category_id"];
    let conn = req.db_conn()?;
    let cat = categories::table
        .filter(categories::slug.eq(::lower(slug)))
        .first::<Category>(&*conn)?;
    let subcats = cat.subcategories(&conn)?
        .into_iter()
        .map(Category::encodable)
        .collect();

    let cat = cat.encodable();
    let cat_with_subcats = EncodableCategoryWithSubcategories {
        id: cat.id,
        category: cat.category,
        slug: cat.slug,
        description: cat.description,
        created_at: cat.created_at,
        crates_cnt: cat.crates_cnt,
        subcategories: subcats,
    };

    #[derive(Serialize)]
    struct R {
        category: EncodableCategoryWithSubcategories,
    }
    Ok(req.json(&R {
        category: cat_with_subcats,
    }))
}

/// Handles the `GET /category_slugs` route.
pub fn slugs(req: &mut Request) -> CargoResult<Response> {
    let conn = req.db_conn()?;
    let slugs = categories::table
        .select((categories::slug, categories::slug))
        .order(categories::slug)
        .load(&*conn)?;

    #[derive(Serialize, Queryable)]
    struct Slug {
        id: String,
        slug: String,
    }

    #[derive(Serialize)]
    struct R {
        category_slugs: Vec<Slug>,
    }
    Ok(req.json(&R {
        category_slugs: slugs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use diesel::connection::SimpleConnection;
    use dotenv::dotenv;
    use serde_json;
    use std::env;

    fn pg_connection() -> PgConnection {
        let _ = dotenv();
        let database_url =
            env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL must be set to run tests");
        let conn = PgConnection::establish(&database_url).unwrap();
        // These tests deadlock if run concurrently
        conn.batch_execute("BEGIN; LOCK categories IN ACCESS EXCLUSIVE MODE")
            .unwrap();
        conn
    }

    #[test]
    fn category_toplevel_excludes_subcategories() {
        let conn = pg_connection();
        conn.batch_execute(
            "INSERT INTO categories (category, slug) VALUES
            ('Cat 2', 'cat2'), ('Cat 1', 'cat1'), ('Cat 1::sub', 'cat1::sub')
            ",
        ).unwrap();

        let categories = Category::toplevel(&conn, "", 10, 0)
            .unwrap()
            .into_iter()
            .map(|c| c.category)
            .collect::<Vec<_>>();
        let expected = vec!["Cat 1".to_string(), "Cat 2".to_string()];
        assert_eq!(expected, categories);
    }

    #[test]
    fn category_toplevel_orders_by_crates_cnt_when_sort_given() {
        let conn = pg_connection();
        conn.batch_execute(
            "INSERT INTO categories (category, slug, crates_cnt) VALUES
            ('Cat 1', 'cat1', 0), ('Cat 2', 'cat2', 2), ('Cat 3', 'cat3', 1)
            ",
        ).unwrap();

        let categories = Category::toplevel(&conn, "crates", 10, 0)
            .unwrap()
            .into_iter()
            .map(|c| c.category)
            .collect::<Vec<_>>();
        let expected = vec![
            "Cat 2".to_string(),
            "Cat 3".to_string(),
            "Cat 1".to_string(),
        ];
        assert_eq!(expected, categories);
    }

    #[test]
    fn category_toplevel_applies_limit_and_offset() {
        let conn = pg_connection();
        conn.batch_execute(
            "INSERT INTO categories (category, slug) VALUES
            ('Cat 1', 'cat1'), ('Cat 2', 'cat2')
            ",
        ).unwrap();

        let categories = Category::toplevel(&conn, "", 1, 0)
            .unwrap()
            .into_iter()
            .map(|c| c.category)
            .collect::<Vec<_>>();
        let expected = vec!["Cat 1".to_string()];
        assert_eq!(expected, categories);

        let categories = Category::toplevel(&conn, "", 1, 1)
            .unwrap()
            .into_iter()
            .map(|c| c.category)
            .collect::<Vec<_>>();
        let expected = vec!["Cat 2".to_string()];
        assert_eq!(expected, categories);
    }

    #[test]
    fn category_toplevel_includes_subcategories_in_crate_cnt() {
        let conn = pg_connection();
        conn.batch_execute(
            "INSERT INTO categories (category, slug, crates_cnt) VALUES
            ('Cat 1', 'cat1', 1), ('Cat 1::sub', 'cat1::sub', 2),
            ('Cat 2', 'cat2', 3), ('Cat 2::Sub 1', 'cat2::sub1', 4),
            ('Cat 2::Sub 2', 'cat2::sub2', 5), ('Cat 3', 'cat3', 6)
            ",
        ).unwrap();

        let categories = Category::toplevel(&conn, "crates", 10, 0)
            .unwrap()
            .into_iter()
            .map(|c| (c.category, c.crates_cnt))
            .collect::<Vec<_>>();
        let expected = vec![
            ("Cat 2".to_string(), 12),
            ("Cat 3".to_string(), 6),
            ("Cat 1".to_string(), 3),
        ];
        assert_eq!(expected, categories);
    }

    #[test]
    fn category_toplevel_applies_limit_and_offset_after_grouping() {
        let conn = pg_connection();
        conn.batch_execute(
            "INSERT INTO categories (category, slug, crates_cnt) VALUES
            ('Cat 1', 'cat1', 1), ('Cat 1::sub', 'cat1::sub', 2),
            ('Cat 2', 'cat2', 3), ('Cat 2::Sub 1', 'cat2::sub1', 4),
            ('Cat 2::Sub 2', 'cat2::sub2', 5), ('Cat 3', 'cat3', 6)
            ",
        ).unwrap();

        let categories = Category::toplevel(&conn, "crates", 2, 0)
            .unwrap()
            .into_iter()
            .map(|c| (c.category, c.crates_cnt))
            .collect::<Vec<_>>();
        let expected = vec![("Cat 2".to_string(), 12), ("Cat 3".to_string(), 6)];
        assert_eq!(expected, categories);

        let categories = Category::toplevel(&conn, "crates", 2, 1)
            .unwrap()
            .into_iter()
            .map(|c| (c.category, c.crates_cnt))
            .collect::<Vec<_>>();
        let expected = vec![("Cat 3".to_string(), 6), ("Cat 1".to_string(), 3)];
        assert_eq!(expected, categories);
    }

    #[test]
    fn category_dates_serializes_to_rfc3339() {
        let cat = EncodableCategory {
            id: "".to_string(),
            category: "".to_string(),
            slug: "".to_string(),
            description: "".to_string(),
            crates_cnt: 1,
            created_at: NaiveDate::from_ymd(2017, 1, 6).and_hms(14, 23, 11),
        };
        let json = serde_json::to_string(&cat).unwrap();
        assert!(
            json.as_str()
                .find(r#""created_at":"2017-01-06T14:23:11+00:00""#)
                .is_some()
        );
    }

    #[test]
    fn category_with_sub_dates_serializes_to_rfc3339() {
        let cat = EncodableCategoryWithSubcategories {
            id: "".to_string(),
            category: "".to_string(),
            slug: "".to_string(),
            description: "".to_string(),
            crates_cnt: 1,
            created_at: NaiveDate::from_ymd(2017, 1, 6).and_hms(14, 23, 11),
            subcategories: vec![],
        };
        let json = serde_json::to_string(&cat).unwrap();
        assert!(
            json.as_str()
                .find(r#""created_at":"2017-01-06T14:23:11+00:00""#)
                .is_some()
        );
    }

}
