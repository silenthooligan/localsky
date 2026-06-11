// <ListItem/>, leading icon + title + optional subtitle + trailing slot
// (control or chevron). The backbone of the new tabbed Settings rows and
// any "label → value/action" list. If `href` is set it renders a
// navigable <a> with a chevron; otherwise a plain row whose trailing
// `children` hold a control (Toggle, Button, value text, …).

use leptos::prelude::*;

use crate::components::ui::Icon;

#[component]
pub fn ListItem(
    #[prop(into)] title: String,
    #[prop(into, optional)] subtitle: String,
    /// Optional leading icon name.
    #[prop(into, optional)]
    icon: Option<&'static str>,
    /// If set, the whole row is a link and shows a chevron.
    #[prop(into, optional)]
    href: Option<String>,
    /// Trailing control slot (rendered right-aligned). Ignored when href set
    /// is false only if empty.
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    let has_sub = !subtitle.is_empty();
    let lead =
        icon.map(|n| view! { <span class="ui-list-item__icon"><Icon name=n size=18/></span> });
    let body = view! {
        {lead}
        <span class="ui-list-item__text">
            <span class="ui-list-item__title">{title.clone()}</span>
            {has_sub.then(|| view! { <span class="ui-list-item__subtitle">{subtitle.clone()}</span> })}
        </span>
    };

    if let Some(href) = href {
        view! {
            <a class="ui-list-item ui-list-item--link" href=href>
                {body}
                <span class="ui-list-item__trail">
                    {children.map(|c| c())}
                    <Icon name="chevron-right" size=18/>
                </span>
            </a>
        }
        .into_any()
    } else {
        view! {
            <div class="ui-list-item">
                {body}
                {children.map(|c| view! { <span class="ui-list-item__trail">{c()}</span> })}
            </div>
        }
        .into_any()
    }
}
