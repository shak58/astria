// This file is @generated by prost-build.
/// CurrencyPair is the standard representation of a pair of assets, where one
/// (Base) is priced in terms of the other (Quote)
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CurrencyPair {
    #[prost(string, tag = "1")]
    pub base: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub quote: ::prost::alloc::string::String,
}
impl ::prost::Name for CurrencyPair {
    const NAME: &'static str = "CurrencyPair";
    const PACKAGE: &'static str = "connect.types.v2";
    fn full_name() -> ::prost::alloc::string::String {
        "connect.types.v2.CurrencyPair".into()
    }
    fn type_url() -> ::prost::alloc::string::String {
        "/connect.types.v2.CurrencyPair".into()
    }
}
