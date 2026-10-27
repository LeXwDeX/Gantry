//! 订阅清单（FA §9.3）：单一常量表，`tests/webhook.rs` 逐行对照文档。
//!
//! 不在表里的 `(event, action)` 落盘前丢弃并计数（forge.md §7.4）。`ping` 只验签，不在表里。

/// `actions` 为空表示该事件没有 `action` 字段或全部接收。
pub struct Subscription {
    pub event: &'static str,
    pub actions: &'static [&'static str],
}

pub const SUBSCRIPTIONS: &[Subscription] = &[
    Subscription {
        event: "pull_request",
        actions: &[
            "opened",
            "reopened",
            "synchronize",
            "closed",
            "edited",
            "ready_for_review",
            "converted_to_draft",
            "labeled",
            "unlabeled",
            "review_requested",
            "review_request_removed",
            "enqueued",
            "dequeued",
        ],
    },
    Subscription {
        event: "pull_request_review",
        actions: &["submitted", "dismissed"],
    },
    Subscription {
        event: "pull_request_review_comment",
        actions: &["created"],
    },
    Subscription {
        event: "pull_request_review_thread",
        actions: &["resolved", "unresolved"],
    },
    Subscription {
        event: "issue_comment",
        actions: &["created", "edited"],
    },
    // 文档列到 `assigned …`；省略号后的 action 需核实，先补上对称的 `unassigned`。
    Subscription {
        event: "issues",
        actions: &[
            "closed",
            "reopened",
            "edited",
            "labeled",
            "unlabeled",
            "assigned",
            "unassigned",
        ],
    },
    Subscription {
        event: "check_suite",
        actions: &["completed"],
    },
    Subscription {
        event: "check_run",
        actions: &["completed"],
    },
    Subscription {
        event: "merge_group",
        actions: &["checks_requested", "destroyed"],
    },
    Subscription {
        event: "push",
        actions: &[],
    },
    Subscription {
        event: "repository",
        actions: &["renamed", "archived", "transferred", "edited"],
    },
    Subscription {
        event: "status",
        actions: &[],
    },
    Subscription {
        event: "installation",
        actions: &[],
    },
    Subscription {
        event: "installation_repositories",
        actions: &[],
    },
];

/// 是否接收这一 `(event, action)`。
pub fn accepts(event: &str, action: Option<&str>) -> bool {
    SUBSCRIPTIONS.iter().any(|subscription| {
        subscription.event == event
            && (subscription.actions.is_empty()
                || action.is_some_and(|action| subscription.actions.contains(&action)))
    })
}
