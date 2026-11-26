mod ar;
mod de;
mod en;
mod es;
mod fr;
mod hi;
mod id;
mod it;
mod ja;
mod ko;
mod nl;
mod pl;
mod pt_br;
mod ru;
mod sv;
mod tr;
mod uk;
mod vi;
mod zh_cn;
mod zh_tw;

use super::Catalog;
use rondi::Locale;

pub(super) const fn get(locale: Locale) -> &'static Catalog {
    match locale {
        Locale::En => &en::CATALOG,
        Locale::De => &de::CATALOG,
        Locale::Es => &es::CATALOG,
        Locale::Fr => &fr::CATALOG,
        Locale::It => &it::CATALOG,
        Locale::PtBr => &pt_br::CATALOG,
        Locale::Nl => &nl::CATALOG,
        Locale::Ru => &ru::CATALOG,
        Locale::Uk => &uk::CATALOG,
        Locale::Ja => &ja::CATALOG,
        Locale::Ko => &ko::CATALOG,
        Locale::ZhCn => &zh_cn::CATALOG,
        Locale::ZhTw => &zh_tw::CATALOG,
        Locale::Ar => &ar::CATALOG,
        Locale::Hi => &hi::CATALOG,
        Locale::Pl => &pl::CATALOG,
        Locale::Tr => &tr::CATALOG,
        Locale::Sv => &sv::CATALOG,
        Locale::Vi => &vi::CATALOG,
        Locale::Id => &id::CATALOG,
    }
}
