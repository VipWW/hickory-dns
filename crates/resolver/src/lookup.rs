// Copyright 2015-2023 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// https://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Lookup result from a resolution of ipv4 and ipv6 records with a Resolver.

use std::{
    cmp::min,
    pin::Pin,
    slice::Iter,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures_util::{
    future::{self, Future},
    stream::Stream,
    FutureExt,
};

use crate::{
    caching_client::CachingClient,
    dns_lru::MAX_TTL,
    error::*,
    hosts::Hosts,
    lookup_ip::LookupIpIter,
    name_server::{ConnectionProvider, NameServerPool},
    proto::{
        op::Query,
        rr::{
            rdata::{self, A, AAAA, NS, PTR},
            Name, RData, Record, RecordType,
        },
        xfer::{DnsRequest, DnsRequestOptions, DnsResponse},
        DnsHandle, ProtoError, RetryDnsHandle,
    },
};

#[cfg(feature = "dnssec")]
use crate::proto::{rr::dnssec::Proven, DnssecDnsHandle};

/// Result of a DNS query when querying for any record type supported by the Hickory DNS Proto library.
///
/// For IP resolution see LookupIp, as it has more features for A and AAAA lookups.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lookup {
    query: Query,
    records: Arc<[Record]>,
    valid_until: Instant,
}

impl Lookup {
    /// Return new instance with given rdata and the maximum TTL.
    pub fn from_rdata(query: Query, rdata: RData) -> Self {
        let record = Record::from_rdata(query.name().clone(), MAX_TTL, rdata);
        Self::new_with_max_ttl(query, Arc::from([record]))
    }

    /// Return new instance with given records and the maximum TTL.
    pub fn new_with_max_ttl(query: Query, records: Arc<[Record]>) -> Self {
        let valid_until = Instant::now() + Duration::from_secs(u64::from(MAX_TTL));
        Self {
            query,
            records,
            valid_until,
        }
    }

    /// Return a new instance with the given records and deadline.
    pub fn new_with_deadline(query: Query, records: Arc<[Record]>, valid_until: Instant) -> Self {
        Self {
            query,
            records,
            valid_until,
        }
    }

    /// Returns a reference to the `Query` that was used to produce this result.
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// Returns an iterator over the data of all records returned during the query.
    ///
    /// It may include additional record types beyond the queried type, e.g. CNAME.
    pub fn iter(&self) -> LookupIter<'_> {
        LookupIter(self.records.iter())
    }

    /// Returns a borrowed iterator of the returned data wrapped in a dnssec Proven type
    #[cfg(feature = "dnssec")]
    pub fn dnssec_iter(&self) -> DnssecIter<'_> {
        DnssecIter(self.dnssec_record_iter())
    }

    /// Returns an iterator over all records returned during the query.
    ///
    /// It may include additional record types beyond the queried type, e.g. CNAME.
    pub fn record_iter(&self) -> LookupRecordIter<'_> {
        LookupRecordIter(self.records.iter())
    }

    /// Returns a borrowed iterator of the returned records wrapped in a dnssec Proven type
    #[cfg(feature = "dnssec")]
    pub fn dnssec_record_iter(&self) -> DnssecLookupRecordIter<'_> {
        DnssecLookupRecordIter(self.records.iter())
    }

    /// Returns the `Instant` at which this `Lookup` is no longer valid.
    pub fn valid_until(&self) -> Instant {
        self.valid_until
    }

    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns an slice over all records that were returned during the query, this can include
    ///   additional record types beyond the queried type, e.g. CNAME.
    pub fn records(&self) -> &[Record] {
        self.records.as_ref()
    }

    /// Clones the inner vec, appends the other vec
    pub(crate) fn append(&self, other: Self) -> Self {
        let mut records = Vec::with_capacity(self.len() + other.len());
        records.extend_from_slice(&self.records);
        records.extend_from_slice(&other.records);

        // Choose the sooner deadline of the two lookups.
        let valid_until = min(self.valid_until(), other.valid_until());
        Self::new_with_deadline(self.query.clone(), Arc::from(records), valid_until)
    }

    /// Add new records to this lookup, without creating a new Lookup
    pub fn extend_records(&mut self, other: Vec<Record>) {
        let mut records = Vec::with_capacity(self.len() + other.len());
        records.extend_from_slice(&self.records);
        records.extend(other);
        self.records = Arc::from(records);
    }
}

/// Borrowed view of set of [`RData`]s returned from a Lookup
pub struct LookupIter<'a>(Iter<'a, Record>);

impl<'a> Iterator for LookupIter<'a> {
    type Item = &'a RData;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(Record::data)
    }
}

/// An iterator over record data with all data wrapped in a Proven type for dnssec validation
#[cfg(feature = "dnssec")]
pub struct DnssecIter<'a>(DnssecLookupRecordIter<'a>);

#[cfg(feature = "dnssec")]

impl<'a> Iterator for DnssecIter<'a> {
    type Item = Proven<&'a RData>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|r| r.map(Record::data))
    }
}

/// Borrowed view of set of [`Record`]s returned from a Lookup
pub struct LookupRecordIter<'a>(Iter<'a, Record>);

impl<'a> Iterator for LookupRecordIter<'a> {
    type Item = &'a Record;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// An iterator over record data with all data wrapped in a Proven type for dnssec validation
#[cfg(feature = "dnssec")]
pub struct DnssecLookupRecordIter<'a>(Iter<'a, Record>);

#[cfg(feature = "dnssec")]

impl<'a> Iterator for DnssecLookupRecordIter<'a> {
    type Item = Proven<&'a Record>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(Proven::from)
    }
}

// TODO: consider removing this as it's not a zero-cost abstraction
impl IntoIterator for Lookup {
    type Item = RData;
    type IntoIter = LookupIntoIter;

    /// This is not a free conversion, because the `RData`s are cloned.
    fn into_iter(self) -> Self::IntoIter {
        LookupIntoIter {
            records: Arc::clone(&self.records),
            index: 0,
        }
    }
}

/// Borrowed view of set of [`RData`]s returned from a [`Lookup`].
///
/// This is not a zero overhead `Iterator`, because it clones each [`RData`].
pub struct LookupIntoIter {
    records: Arc<[Record]>,
    index: usize,
}

impl Iterator for LookupIntoIter {
    type Item = RData;

    fn next(&mut self) -> Option<Self::Item> {
        let rdata = self.records.get(self.index).map(Record::data);
        self.index += 1;
        rdata.cloned()
    }
}

/// Different lookup options for the lookup attempts and validation
#[derive(Clone)]
#[doc(hidden)]
pub enum LookupEither<P: ConnectionProvider + Send> {
    Retry(RetryDnsHandle<NameServerPool<P>>),
    #[cfg(feature = "dnssec")]
    Secure(DnssecDnsHandle<RetryDnsHandle<NameServerPool<P>>>),
}

impl<P: ConnectionProvider> DnsHandle for LookupEither<P> {
    type Response = Pin<Box<dyn Stream<Item = Result<DnsResponse, ProtoError>> + Send>>;

    fn is_verifying_dnssec(&self) -> bool {
        match self {
            Self::Retry(c) => c.is_verifying_dnssec(),
            #[cfg(feature = "dnssec")]
            Self::Secure(c) => c.is_verifying_dnssec(),
        }
    }

    fn send<R: Into<DnsRequest> + Unpin + Send + 'static>(&self, request: R) -> Self::Response {
        match self {
            Self::Retry(c) => c.send(request),
            #[cfg(feature = "dnssec")]
            Self::Secure(c) => c.send(request),
        }
    }
}

/// The Future returned from [`AsyncResolver`] when performing a lookup.
#[doc(hidden)]
pub struct LookupFuture<C>
where
    C: DnsHandle + 'static,
{
    client_cache: CachingClient<C>,
    names: Vec<Name>,
    record_type: RecordType,
    options: DnsRequestOptions,
    query: Pin<Box<dyn Future<Output = Result<Lookup, ResolveError>> + Send>>,
}

impl<C> LookupFuture<C>
where
    C: DnsHandle + 'static,
{
    /// Perform a lookup from a name and type to a set of RDatas
    ///
    /// # Arguments
    ///
    /// * `names` - a set of DNS names to attempt to resolve, they will be attempted in queue order, i.e. the first is `names.pop()`. Upon each failure, the next will be attempted.
    /// * `record_type` - type of record being sought
    /// * `client_cache` - cache with a connection to use for performing all lookups
    #[doc(hidden)]
    pub fn lookup(
        names: Vec<Name>,
        record_type: RecordType,
        options: DnsRequestOptions,
        client_cache: CachingClient<C>,
    ) -> Self {
        Self::lookup_with_hosts(names, record_type, options, client_cache, None)
    }

    /// Perform a lookup from a name and type to a set of RDatas, taking the local
    /// hosts file into account.
    ///
    /// # Arguments
    ///
    /// * `names` - a set of DNS names to attempt to resolve, they will be attempted in queue order, i.e. the first is `names.pop()`. Upon each failure, the next will be attempted.
    /// * `record_type` - type of record being sought
    /// * `client_cache` - cache with a connection to use for performing all lookups
    /// * `hosts` - the local host file, the records inside it will be prioritized over the upstream DNS server
    #[doc(hidden)]
    pub fn lookup_with_hosts(
        mut names: Vec<Name>,
        record_type: RecordType,
        options: DnsRequestOptions,
        mut client_cache: CachingClient<C>,
        hosts: Option<Arc<Hosts>>,
    ) -> Self {
        let name = names.pop().ok_or_else(|| {
            ResolveError::from(ResolveErrorKind::Message("can not lookup for no names"))
        });

        let query: Pin<Box<dyn Future<Output = Result<Lookup, ResolveError>> + Send>> = match name {
            Ok(name) => {
                let query = Query::query(name, record_type);

                if let Some(lookup) = hosts.and_then(|h| h.lookup_static_host(&query)) {
                    future::ok(lookup).boxed()
                } else {
                    client_cache.lookup(query, options).boxed()
                }
            }
            Err(err) => future::err(err).boxed(),
        };

        Self {
            client_cache,
            names,
            record_type,
            options,
            query,
        }
    }
}

impl<C> Future for LookupFuture<C>
where
    C: DnsHandle + 'static,
{
    type Output = Result<Lookup, ResolveError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            // Try polling the underlying DNS query.
            let query = self.query.as_mut().poll_unpin(cx);

            // Determine whether or not we will attempt to retry the query.
            let should_retry = match &query {
                // If the query is NotReady, yield immediately.
                Poll::Pending => return Poll::Pending,
                // If the query returned a successful lookup, we will attempt
                // to retry if the lookup is empty. Otherwise, we will return
                // that lookup.
                Poll::Ready(Ok(lookup)) => lookup.records.len() == 0,
                // If the query failed, we will attempt to retry.
                Poll::Ready(Err(_)) => true,
            };

            if should_retry {
                if let Some(name) = self.names.pop() {
                    let record_type = self.record_type;
                    let options = self.options;

                    // If there's another name left to try, build a new query
                    // for that next name and continue looping.
                    self.query = self
                        .client_cache
                        .lookup(Query::query(name, record_type), options);
                    // Continue looping with the new query. It will be polled
                    // on the next iteration of the loop.
                    continue;
                }
            }
            // If we didn't have to retry the query, or we weren't able to
            // retry because we've exhausted the names to search, return the
            // current query.
            return query;
            // If we skipped retrying the  query, this will return the
            // successful lookup, otherwise, if the retry failed, this will
            // return the last  query result --- either an empty lookup or the
            // last error we saw.
        }
    }
}

/// The result of an SRV lookup
#[derive(Debug, Clone)]
pub struct SrvLookup(Lookup);

impl SrvLookup {
    /// Returns an iterator over the SRV RData
    pub fn iter(&self) -> SrvLookupIter<'_> {
        SrvLookupIter(self.0.iter())
    }

    /// Returns a reference to the Query that was used to produce this result.
    pub fn query(&self) -> &Query {
        self.0.query()
    }

    /// Returns the list of IPs associated with the SRV record.
    ///
    /// *Note*: That Hickory DNS performs a recursive lookup on SRV records for IPs if they were not included in the original request. If there are no IPs associated to the result, a subsequent query for the IPs via the `srv.target()` should not resolve to the IPs.
    pub fn ip_iter(&self) -> LookupIpIter<'_> {
        LookupIpIter(self.0.iter())
    }

    /// Return a reference to the inner lookup
    ///
    /// This can be useful for getting all records from the request
    pub fn as_lookup(&self) -> &Lookup {
        &self.0
    }
}

impl From<Lookup> for SrvLookup {
    fn from(lookup: Lookup) -> Self {
        Self(lookup)
    }
}

/// An iterator over the Lookup type
pub struct SrvLookupIter<'i>(LookupIter<'i>);

impl<'i> Iterator for SrvLookupIter<'i> {
    type Item = &'i rdata::SRV;

    fn next(&mut self) -> Option<Self::Item> {
        let iter: &mut _ = &mut self.0;
        iter.find_map(|rdata| match rdata {
            RData::SRV(data) => Some(data),
            _ => None,
        })
    }
}

impl IntoIterator for SrvLookup {
    type Item = rdata::SRV;
    type IntoIter = SrvLookupIntoIter;

    /// This is not a free conversion, because the `RData`s are cloned.
    fn into_iter(self) -> Self::IntoIter {
        SrvLookupIntoIter(self.0.into_iter())
    }
}

/// Borrowed view of set of RDatas returned from a Lookup
pub struct SrvLookupIntoIter(LookupIntoIter);

impl Iterator for SrvLookupIntoIter {
    type Item = rdata::SRV;

    fn next(&mut self) -> Option<Self::Item> {
        let iter: &mut _ = &mut self.0;
        iter.find_map(|rdata| match rdata {
            RData::SRV(data) => Some(data),
            _ => None,
        })
    }
}

/// Creates a Lookup result type from the specified components
macro_rules! lookup_type {
    ($l:ident, $i:ident, $ii:ident, $r:path, $t:path) => {
        /// Contains the results of a lookup for the associated RecordType
        #[derive(Debug, Clone)]
        pub struct $l(Lookup);

        impl $l {
            #[doc = stringify!(Returns an iterator over the records that match $r)]
            pub fn iter(&self) -> $i<'_> {
                $i(self.0.iter())
            }

            /// Returns a reference to the Query that was used to produce this result.
            pub fn query(&self) -> &Query {
                self.0.query()
            }

            /// Returns the `Instant` at which this result is no longer valid.
            pub fn valid_until(&self) -> Instant {
                self.0.valid_until()
            }

            /// Return a reference to the inner lookup
            ///
            /// This can be useful for getting all records from the request
            pub fn as_lookup(&self) -> &Lookup {
                &self.0
            }
        }

        impl From<Lookup> for $l {
            fn from(lookup: Lookup) -> Self {
                $l(lookup)
            }
        }

        impl From<$l> for Lookup {
            fn from(revlookup: $l) -> Self {
                revlookup.0
            }
        }

        /// An iterator over the Lookup type
        pub struct $i<'i>(LookupIter<'i>);

        impl<'i> Iterator for $i<'i> {
            type Item = &'i $t;

            fn next(&mut self) -> Option<Self::Item> {
                let iter: &mut _ = &mut self.0;
                iter.find_map(|rdata| match rdata {
                    $r(data) => Some(data),
                    _ => None,
                })
            }
        }

        impl IntoIterator for $l {
            type Item = $t;
            type IntoIter = $ii;

            /// This is not a free conversion, because the `RData`s are cloned.
            fn into_iter(self) -> Self::IntoIter {
                $ii(self.0.into_iter())
            }
        }

        /// Borrowed view of set of RDatas returned from a Lookup
        pub struct $ii(LookupIntoIter);

        impl Iterator for $ii {
            type Item = $t;

            fn next(&mut self) -> Option<Self::Item> {
                let iter: &mut _ = &mut self.0;
                iter.find_map(|rdata| match rdata {
                    $r(data) => Some(data),
                    _ => None,
                })
            }
        }
    };
}

// Generate all Lookup record types
lookup_type!(
    ReverseLookup,
    ReverseLookupIter,
    ReverseLookupIntoIter,
    RData::PTR,
    PTR
);
lookup_type!(Ipv4Lookup, Ipv4LookupIter, Ipv4LookupIntoIter, RData::A, A);
lookup_type!(
    Ipv6Lookup,
    Ipv6LookupIter,
    Ipv6LookupIntoIter,
    RData::AAAA,
    AAAA
);
lookup_type!(
    MxLookup,
    MxLookupIter,
    MxLookupIntoIter,
    RData::MX,
    rdata::MX
);
lookup_type!(
    TlsaLookup,
    TlsaLookupIter,
    TlsaLookupIntoIter,
    RData::TLSA,
    rdata::TLSA
);
lookup_type!(
    TxtLookup,
    TxtLookupIter,
    TxtLookupIntoIter,
    RData::TXT,
    rdata::TXT
);
lookup_type!(
    CertLookup,
    CertLookupIter,
    CertLookupIntoIter,
    RData::CERT,
    rdata::CERT
);
lookup_type!(
    SoaLookup,
    SoaLookupIter,
    SoaLookupIntoIter,
    RData::SOA,
    rdata::SOA
);
lookup_type!(NsLookup, NsLookupIter, NsLookupIntoIter, RData::NS, NS);

#[cfg(test)]
pub mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    use futures_executor::block_on;
    use futures_util::future;
    use futures_util::stream::once;

    use crate::proto::op::{Message, Query};
    use crate::proto::rr::{Name, RData, Record, RecordType};
    use crate::proto::xfer::{DnsRequest, DnsRequestOptions};
    use crate::proto::{ProtoError, ProtoErrorKind};

    use super::*;

    #[derive(Clone)]
    pub struct MockDnsHandle {
        messages: Arc<Mutex<Vec<Result<DnsResponse, ProtoError>>>>,
    }

    impl DnsHandle for MockDnsHandle {
        type Response = Pin<Box<dyn Stream<Item = Result<DnsResponse, ProtoError>> + Send>>;

        fn send<R: Into<DnsRequest>>(&self, _: R) -> Self::Response {
            Box::pin(once(
                future::ready(self.messages.lock().unwrap().pop().unwrap_or_else(empty)).boxed(),
            ))
        }
    }

    pub fn v4_message() -> Result<DnsResponse, ProtoError> {
        let mut message = Message::new();
        message.add_query(Query::query(Name::root(), RecordType::A));
        message.insert_answers(vec![Record::from_rdata(
            Name::root(),
            86400,
            RData::A(A::new(127, 0, 0, 1)),
        )]);

        let resp = DnsResponse::from_message(message).unwrap();
        assert!(resp.contains_answer());
        Ok(resp)
    }

    pub fn empty() -> Result<DnsResponse, ProtoError> {
        Ok(DnsResponse::from_message(Message::new()).unwrap())
    }

    pub fn error() -> Result<DnsResponse, ProtoError> {
        Err(ProtoError::from(std::io::Error::from(
            std::io::ErrorKind::Other,
        )))
    }

    pub fn mock(messages: Vec<Result<DnsResponse, ProtoError>>) -> MockDnsHandle {
        MockDnsHandle {
            messages: Arc::new(Mutex::new(messages)),
        }
    }

    #[test]
    fn test_lookup() {
        assert_eq!(
            block_on(LookupFuture::lookup(
                vec![Name::root()],
                RecordType::A,
                DnsRequestOptions::default(),
                CachingClient::new(0, mock(vec![v4_message()]), false),
            ))
            .unwrap()
            .iter()
            .map(|r| r.ip_addr().unwrap())
            .collect::<Vec<IpAddr>>(),
            vec![Ipv4Addr::LOCALHOST]
        );
    }

    #[test]
    fn test_lookup_slice() {
        assert_eq!(
            Record::data(
                &block_on(LookupFuture::lookup(
                    vec![Name::root()],
                    RecordType::A,
                    DnsRequestOptions::default(),
                    CachingClient::new(0, mock(vec![v4_message()]), false),
                ))
                .unwrap()
                .records()[0]
            )
            .ip_addr()
            .unwrap(),
            Ipv4Addr::LOCALHOST
        );
    }

    #[test]
    fn test_lookup_into_iter() {
        assert_eq!(
            block_on(LookupFuture::lookup(
                vec![Name::root()],
                RecordType::A,
                DnsRequestOptions::default(),
                CachingClient::new(0, mock(vec![v4_message()]), false),
            ))
            .unwrap()
            .into_iter()
            .map(|r| r.ip_addr().unwrap())
            .collect::<Vec<IpAddr>>(),
            vec![Ipv4Addr::LOCALHOST]
        );
    }

    #[test]
    fn test_error() {
        assert!(block_on(LookupFuture::lookup(
            vec![Name::root()],
            RecordType::A,
            DnsRequestOptions::default(),
            CachingClient::new(0, mock(vec![error()]), false),
        ))
        .is_err());
    }

    #[test]
    fn test_empty_no_response() {
        if let ProtoErrorKind::NoRecordsFound {
            query,
            negative_ttl,
            ..
        } = block_on(LookupFuture::lookup(
            vec![Name::root()],
            RecordType::A,
            DnsRequestOptions::default(),
            CachingClient::new(0, mock(vec![empty()]), false),
        ))
        .expect_err("this should have been a NoRecordsFound")
        .proto()
        .expect("it should have been a ProtoError")
        .kind()
        {
            assert_eq!(**query, Query::query(Name::root(), RecordType::A));
            assert_eq!(*negative_ttl, None);
        } else {
            panic!("wrong error received");
        }
    }

    #[test]
    fn test_lookup_into_iter_arc() {
        let mut lookup = LookupIntoIter {
            records: Arc::from([
                Record::from_rdata(
                    Name::from_str("www.example.com.").unwrap(),
                    80,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                Record::from_rdata(
                    Name::from_str("www.example.com.").unwrap(),
                    80,
                    RData::A(A::new(127, 0, 0, 2)),
                ),
            ]),
            index: 0,
        };

        assert_eq!(lookup.next().unwrap(), RData::A(A::new(127, 0, 0, 1)));
        assert_eq!(lookup.next().unwrap(), RData::A(A::new(127, 0, 0, 2)));
        assert_eq!(lookup.next(), None);
    }

    #[test]
    #[cfg(feature = "dnssec")]
    fn test_dnssec_lookup() {
        use hickory_proto::rr::dnssec::Proof;

        let mut a1 = Record::from_rdata(
            Name::from_str("www.example.com.").unwrap(),
            80,
            RData::A(A::new(127, 0, 0, 1)),
        );
        a1.set_proof(Proof::Secure);

        let mut a2 = Record::from_rdata(
            Name::from_str("www.example.com.").unwrap(),
            80,
            RData::A(A::new(127, 0, 0, 2)),
        );
        a2.set_proof(Proof::Insecure);

        let lookup = Lookup {
            query: Query::default(),
            records: Arc::from([a1.clone(), a2.clone()]),
            valid_until: Instant::now(),
        };

        let mut lookup = lookup.dnssec_iter();

        assert_eq!(
            *lookup.next().unwrap().require(Proof::Secure).unwrap(),
            *a1.data()
        );
        assert_eq!(
            *lookup.next().unwrap().require(Proof::Insecure).unwrap(),
            *a2.data()
        );
        assert_eq!(lookup.next(), None);
    }
}
