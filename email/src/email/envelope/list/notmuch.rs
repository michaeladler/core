use async_trait::async_trait;
use chrono::TimeDelta;
use notmuch::Sort;
use tracing::{debug, info, trace};

use super::{Envelopes, ListEnvelopes, ListEnvelopesOptions};
use crate::{
    email::error::Error,
    envelope::Envelope,
    notmuch::{build_folder_query, NotmuchContextSync},
    search_query::{filter::SearchEmailsFilterQuery, SearchEmailsQuery},
    AnyResult,
};

#[derive(Clone)]
pub struct ListNotmuchEnvelopes {
    ctx: NotmuchContextSync,
}

impl ListNotmuchEnvelopes {
    pub fn new(ctx: &NotmuchContextSync) -> Self {
        Self { ctx: ctx.clone() }
    }

    pub fn new_boxed(ctx: &NotmuchContextSync) -> Box<dyn ListEnvelopes> {
        Box::new(Self::new(ctx))
    }

    pub fn some_new_boxed(ctx: &NotmuchContextSync) -> Option<Box<dyn ListEnvelopes>> {
        Some(Self::new_boxed(ctx))
    }
}

#[async_trait]
impl ListEnvelopes for ListNotmuchEnvelopes {
    async fn list_envelopes(
        &self,
        folder: &str,
        opts: ListEnvelopesOptions,
    ) -> AnyResult<Envelopes> {
        info!("listing notmuch envelopes from folder {folder}");

        let ctx = self.ctx.lock().await;
        let config = &ctx.account_config;
        let db = ctx.open_db()?;

        let ref folder = config.get_folder_alias(folder);
        let mut final_query = build_folder_query(config, ctx.maildirpp(), folder);

        if let Some(query) = opts.query.as_ref() {
            let query = query.to_notmuch_search_query();
            if !query.is_empty() {
                if final_query == "*" {
                    final_query.clear();
                } else {
                    final_query.push_str(" and ");
                }
                final_query.push_str(&query);
            }
        }

        let query_builder = db
            .create_query(&final_query)
            .map_err(Error::NotMuchFailure)?;

        // Ask notmuch to sort by newest-first server-side. This matches
        // the default fallback ordering used by `sort_envelopes` (date
        // descending) and lets us paginate the message iterator without
        // materializing every matching message.
        query_builder.set_sort(Sort::NewestFirst);

        let has_custom_sort = opts
            .query
            .as_ref()
            .and_then(|q| q.sort.as_ref())
            .map(|s| !s.is_empty())
            .unwrap_or(false);

        let page_begin = opts.page * opts.page_size;

        let mut envelopes = if has_custom_sort {
            // Custom sort (by from/to/subject/...) requires every envelope
            // to be materialized so we can sort in Rust.
            let msgs = query_builder.search_messages().map_err(|err| {
                Error::SearchMessagesInvalidQueryNotmuch(
                    err,
                    folder.to_owned(),
                    final_query.clone(),
                )
            })?;
            let mut envelopes = Envelopes::from_notmuch_msgs(msgs);

            debug!(
                "found {} notmuch envelopes matching query {final_query}",
                envelopes.len()
            );
            trace!("{envelopes:#?}");

            if page_begin > envelopes.len() {
                return Err(Error::GetEnvelopesOutOfBoundsNotmuchError(
                    folder.to_owned(),
                    page_begin + 1,
                ))?;
            }

            opts.sort_envelopes(&mut envelopes);

            let page_end = envelopes.len().min(if opts.page_size == 0 {
                envelopes.len()
            } else {
                page_begin + opts.page_size
            });

            *envelopes = envelopes[page_begin..page_end].into();
            envelopes
        } else {
            // Default sort (date desc): use notmuch's native sort and only
            // build envelopes for the requested page. Out-of-bounds is
            // checked using a fast count query.
            let total = query_builder
                .count_messages()
                .map_err(Error::NotMuchFailure)? as usize;

            debug!("found {total} notmuch envelopes matching query {final_query}");

            if page_begin > total {
                return Err(Error::GetEnvelopesOutOfBoundsNotmuchError(
                    folder.to_owned(),
                    page_begin + 1,
                ))?;
            }

            let msgs = query_builder.search_messages().map_err(|err| {
                Error::SearchMessagesInvalidQueryNotmuch(
                    err,
                    folder.to_owned(),
                    final_query.clone(),
                )
            })?;

            let envelopes: Envelopes = if opts.page_size == 0 {
                msgs.skip(page_begin)
                    .map(Envelope::from_notmuch_msg)
                    .collect()
            } else {
                msgs.skip(page_begin)
                    .take(opts.page_size)
                    .map(Envelope::from_notmuch_msg)
                    .collect()
            };

            trace!("{envelopes:#?}");
            envelopes
        };

        // Ensure final ordering matches the requested sort (no-op when
        // notmuch already delivered them in the right order).
        opts.sort_envelopes(&mut envelopes);

        db.close().map_err(Error::NotMuchFailure)?;

        Ok(envelopes)
    }
}

impl SearchEmailsQuery {
    pub fn to_notmuch_search_query(&self) -> String {
        self.filter
            .as_ref()
            .map(|f| f.to_notmuch_search_query())
            .unwrap_or_default()
    }
}

impl SearchEmailsFilterQuery {
    pub fn to_notmuch_search_query(&self) -> String {
        let mut query = String::new();

        match self {
            SearchEmailsFilterQuery::And(left, right) => {
                query.push_str("(");
                query.push_str(&left.to_notmuch_search_query());
                query.push_str(") and (");
                query.push_str(&right.to_notmuch_search_query());
                query.push(')');
            }
            SearchEmailsFilterQuery::Or(left, right) => {
                query.push_str("(");
                query.push_str(&left.to_notmuch_search_query());
                query.push_str(") or (");
                query.push_str(&right.to_notmuch_search_query());
                query.push(')');
            }
            SearchEmailsFilterQuery::Not(right) => {
                query.push_str("not (");
                query.push_str(&right.to_notmuch_search_query());
                query.push_str(")");
            }
            SearchEmailsFilterQuery::Date(date) => {
                query.push_str("date:");
                query.push_str(&date.to_string());
            }
            SearchEmailsFilterQuery::BeforeDate(date) => {
                // notmuch dates are inclusive, so we substract one
                // day from the before date filter.
                let date = *date - TimeDelta::try_days(1).unwrap();
                query.push_str("date:..");
                query.push_str(&date.to_string());
            }
            SearchEmailsFilterQuery::AfterDate(date) => {
                // notmuch dates are inclusive, so we add one day to
                // the after date filter.
                let date = *date + TimeDelta::try_days(1).unwrap();
                query.push_str("date:");
                query.push_str(&date.to_string());
                query.push_str("..");
            }
            SearchEmailsFilterQuery::From(pattern) => {
                query.push_str("from:/");
                query.push_str(pattern);
                query.push('/');
            }

            SearchEmailsFilterQuery::To(pattern) => {
                query.push_str("to:/");
                query.push_str(pattern);
                query.push('/');
            }
            SearchEmailsFilterQuery::Subject(pattern) => {
                query.push_str("subject:");
                query.push_str(pattern);
            }
            SearchEmailsFilterQuery::Body(pattern) => {
                query.push_str("body:");
                query.push_str(pattern);
            }
            SearchEmailsFilterQuery::Flag(flag) => {
                query.push_str("tag:");
                query.push_str(&flag.to_string());
            }
        };

        query
    }
}
