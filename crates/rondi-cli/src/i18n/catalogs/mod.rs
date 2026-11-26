mod af;
mod am;
mod ar;
mod bg;
mod bn;
mod ca;
mod cs;
mod da;
mod de;
mod el;
mod en;
mod es;
mod et;
mod fa;
mod fi;
mod fil;
mod fr;
mod gu;
mod ha;
mod he;
mod hi;
mod hr;
mod hu;
mod id;
mod ig;
mod it;
mod ja;
mod ka;
mod kn;
mod ko;
mod lt;
mod lv;
mod mg;
mod ml;
mod mr;
mod ms;
mod nb;
mod ne;
mod nl;
mod pa;
mod pl;
mod pt_br;
mod pt_pt;
mod ro;
mod ru;
mod rw;
mod sk;
mod sl;
mod so;
mod sr;
mod sv;
mod sw;
mod ta;
mod te;
mod th;
mod tr;
mod uk;
mod ur;
mod vi;
mod xh;
mod yo;
mod zh_cn;
mod zh_tw;
mod zu;

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
        Locale::Af => &af::CATALOG,
        Locale::Am => &am::CATALOG,
        Locale::Ha => &ha::CATALOG,
        Locale::Ig => &ig::CATALOG,
        Locale::Mg => &mg::CATALOG,
        Locale::Rw => &rw::CATALOG,
        Locale::So => &so::CATALOG,
        Locale::Sw => &sw::CATALOG,
        Locale::Xh => &xh::CATALOG,
        Locale::Yo => &yo::CATALOG,
        Locale::Zu => &zu::CATALOG,
        Locale::Fa => &fa::CATALOG,
        Locale::He => &he::CATALOG,
        Locale::Bn => &bn::CATALOG,
        Locale::Mr => &mr::CATALOG,
        Locale::Ta => &ta::CATALOG,
        Locale::Te => &te::CATALOG,
        Locale::Th => &th::CATALOG,
        Locale::Ur => &ur::CATALOG,
        Locale::Bg => &bg::CATALOG,
        Locale::El => &el::CATALOG,
        Locale::Lv => &lv::CATALOG,
        Locale::PtPt => &pt_pt::CATALOG,
        Locale::Cs => &cs::CATALOG,
        Locale::Da => &da::CATALOG,
        Locale::Fi => &fi::CATALOG,
        Locale::Hu => &hu::CATALOG,
        Locale::Ro => &ro::CATALOG,
        Locale::Sk => &sk::CATALOG,
        Locale::Nb => &nb::CATALOG,
        Locale::Ca => &ca::CATALOG,
        Locale::Hr => &hr::CATALOG,
        Locale::Sr => &sr::CATALOG,
        Locale::Sl => &sl::CATALOG,
        Locale::Lt => &lt::CATALOG,
        Locale::Et => &et::CATALOG,
        Locale::Ka => &ka::CATALOG,
        Locale::Ms => &ms::CATALOG,
        Locale::Fil => &fil::CATALOG,
        Locale::Ne => &ne::CATALOG,
        Locale::Pa => &pa::CATALOG,
        Locale::Gu => &gu::CATALOG,
        Locale::Kn => &kn::CATALOG,
        Locale::Ml => &ml::CATALOG,
    }
}

/// Whether `locale`'s catalog is still an untranslated copy of the English
/// catalog. Each such catalog file starts with `// TRANSLATION-PENDING`;
/// remove the locale from this list when its translation lands.
#[cfg(test)]
pub(super) const fn translation_pending(locale: Locale) -> bool {
    // Every shipped catalog is translated; keep the hook for new locales.
    let _ = locale;
    false
}
