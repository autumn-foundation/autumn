pub mod docs;
pub mod pages;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_generates_html() {
        use autumn_web::prelude::*;
        let markup = pages::layout("Test Title", html! { p { "Test Content" } });
        let html_string = markup.into_string();
        assert!(html_string.contains("Test Title — Wiki"));
        assert!(html_string.contains("Test Content"));
    }

    #[test]
    fn test_status_badge() {
        let markup = pages::status_badge("published");
        let html_string = markup.into_string();
        assert!(html_string.contains("published"));
        assert!(html_string.contains("bg-green-100"));

        let markup = pages::status_badge("draft");
        let html_string = markup.into_string();
        assert!(html_string.contains("draft"));
        assert!(html_string.contains("bg-yellow-100"));

        let markup = pages::status_badge("archived");
        let html_string = markup.into_string();
        assert!(html_string.contains("archived"));
        assert!(html_string.contains("bg-gray-200"));
    }

    #[test]
    fn test_path_helpers() {
        assert_eq!(pages::__autumn_path_list(), "/");
        assert_eq!(
            pages::__autumn_path_show("hello".to_owned()),
            "/pages/hello"
        );
        assert_eq!(
            pages::__autumn_path_edit_form("hello".to_owned()),
            "/pages/hello/edit"
        );
        assert_eq!(
            pages::__autumn_path_history("hello".to_owned()),
            "/pages/hello/history"
        );
    }

    #[test]
    fn test_new_form() {
        let _runtime = tokio::runtime::Runtime::new().unwrap();
        let markup = _runtime.block_on(async { pages::new_form().await });
        let html_string = markup.into_string();
        assert!(html_string.contains("New Page"));
    }

    #[test]
    fn test_page_form_into_update() {
        use crate::routes::pages::PageForm;
        let form = PageForm {
            title: "Test Title".into(),
            body: "Test Body".into(),
            status: "published".into(),
            lock_version: 3,
        };

        let update_page = form.into_update();
        if let autumn_web::Patch::Set(title) = update_page.title {
            assert_eq!(title, "Test Title");
        } else {
            panic!("Expected title to be Set");
        }
    }

    #[test]
    fn test_pages_list_snippet() {
        use crate::models::Page;
        let p1 = Page {
            id: 1,
            slug: "test-slug-1".into(),
            title: "Test Title 1".into(),
            body: "Test Body".into(),
            status: "published".into(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            lock_version: 1,
        };
        let p2 = Page {
            id: 2,
            slug: "test-slug-2".into(),
            title: "Test Title 2".into(),
            body: "Test Body".into(),
            status: "draft".into(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            lock_version: 1,
        };
        let pages = vec![p1, p2];
        let markup = pages::pages_list_snippet(&pages);
        let html_string = markup.into_string();

        assert!(html_string.contains("test-slug-1"));
        assert!(html_string.contains("Test Title 1"));
        assert!(html_string.contains("published"));

        assert!(html_string.contains("test-slug-2"));
        assert!(html_string.contains("Test Title 2"));
        assert!(html_string.contains("draft"));
    }

    #[test]
    fn test_update_summary_generation() {
        use crate::routes::pages::generate_update_summary;

        // Status change takes precedence
        assert_eq!(
            generate_update_summary("draft", "published", "Title", "Title"),
            Some("Status changed: draft → published".to_string())
        );

        // Title change
        assert_eq!(
            generate_update_summary("draft", "draft", "Old Title", "New Title"),
            Some("Title changed: Old Title → New Title".to_string())
        );

        // Both changed (status takes precedence)
        assert_eq!(
            generate_update_summary("draft", "published", "Old Title", "New Title"),
            Some("Status changed: draft → published".to_string())
        );

        // No change
        assert_eq!(
            generate_update_summary("draft", "draft", "Title", "Title"),
            None
        );
    }
}
