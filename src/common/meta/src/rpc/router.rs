// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};

use api::v1::meta::{
    CreateRequest as PbCreateRequest, DeleteRequest as PbDeleteRequest, Partition as PbPartition,
    Peer as PbPeer, Region as PbRegion, RegionRoute as PbRegionRoute,
    RouteRequest as PbRouteRequest, RouteResponse as PbRouteResponse, Table as PbTable,
    TableRoute as PbTableRoute,
};
use serde::{Deserialize, Serialize, Serializer};
use snafu::{OptionExt, ResultExt};
use table::metadata::RawTableInfo;

use crate::error::{self, Result};
use crate::peer::Peer;
use crate::rpc::util;
use crate::table_name::TableName;

#[derive(Debug, Clone)]
pub struct CreateRequest<'a> {
    pub table_name: TableName,
    pub partitions: Vec<Partition>,
    pub table_info: &'a RawTableInfo,
}

impl TryFrom<CreateRequest<'_>> for PbCreateRequest {
    type Error = error::Error;

    fn try_from(mut req: CreateRequest) -> Result<Self> {
        Ok(Self {
            header: None,
            table_name: Some(req.table_name.into()),
            partitions: req.partitions.drain(..).map(Into::into).collect(),
            table_info: serde_json::to_vec(&req.table_info).context(error::SerdeJsonSnafu)?,
        })
    }
}

impl<'a> CreateRequest<'a> {
    #[inline]
    pub fn new(table_name: TableName, table_info: &'a RawTableInfo) -> Self {
        Self {
            table_name,
            partitions: vec![],
            table_info,
        }
    }

    #[inline]
    pub fn add_partition(mut self, partition: Partition) -> Self {
        self.partitions.push(partition);
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct RouteRequest {
    pub table_names: Vec<TableName>,
}

impl From<RouteRequest> for PbRouteRequest {
    fn from(mut req: RouteRequest) -> Self {
        Self {
            header: None,
            table_names: req.table_names.drain(..).map(Into::into).collect(),
        }
    }
}

impl RouteRequest {
    #[inline]
    pub fn new() -> Self {
        Self {
            table_names: vec![],
        }
    }

    #[inline]
    pub fn add_table_name(mut self, table_name: TableName) -> Self {
        self.table_names.push(table_name);
        self
    }
}

#[derive(Debug, Clone)]
pub struct DeleteRequest {
    pub table_name: TableName,
}

impl From<DeleteRequest> for PbDeleteRequest {
    fn from(req: DeleteRequest) -> Self {
        Self {
            header: None,
            table_name: Some(req.table_name.into()),
        }
    }
}

impl DeleteRequest {
    #[inline]
    pub fn new(table_name: TableName) -> Self {
        Self { table_name }
    }
}

#[derive(Debug, Clone)]
pub struct RouteResponse {
    pub table_routes: Vec<TableRoute>,
}

impl TryFrom<PbRouteResponse> for RouteResponse {
    type Error = error::Error;

    fn try_from(pb: PbRouteResponse) -> Result<Self> {
        util::check_response_header(pb.header.as_ref())?;

        let table_routes = pb
            .table_routes
            .into_iter()
            .map(|x| TableRoute::try_from_raw(&pb.peers, x))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { table_routes })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct TableRoute {
    pub table: Table,
    pub region_routes: Vec<RegionRoute>,
}

impl TableRoute {
    pub fn try_from_raw(peers: &[PbPeer], table_route: PbTableRoute) -> Result<Self> {
        let table = table_route
            .table
            .context(error::RouteInfoCorruptedSnafu {
                err_msg: "'table' is empty in table route",
            })?
            .try_into()?;

        let mut region_routes = Vec::with_capacity(table_route.region_routes.len());
        for region_route in table_route.region_routes.into_iter() {
            let region = region_route
                .region
                .context(error::RouteInfoCorruptedSnafu {
                    err_msg: "'region' is empty in region route",
                })?
                .into();

            let leader_peer = peers
                .get(region_route.leader_peer_index as usize)
                .cloned()
                .map(Into::into);

            let follower_peers = region_route
                .follower_peer_indexes
                .into_iter()
                .filter_map(|x| peers.get(x as usize).cloned().map(Into::into))
                .collect::<Vec<_>>();

            region_routes.push(RegionRoute {
                region,
                leader_peer,
                follower_peers,
            });
        }

        Ok(Self {
            table,
            region_routes,
        })
    }

    pub fn try_into_raw(self) -> Result<(Vec<PbPeer>, PbTableRoute)> {
        let mut peers = HashSet::new();
        self.region_routes
            .iter()
            .filter_map(|x| x.leader_peer.as_ref())
            .for_each(|p| {
                peers.insert(p.clone());
            });
        self.region_routes
            .iter()
            .flat_map(|x| x.follower_peers.iter())
            .for_each(|p| {
                peers.insert(p.clone());
            });
        let mut peers = peers.into_iter().map(Into::into).collect::<Vec<PbPeer>>();
        peers.sort_by_key(|x| x.id);

        let find_peer = |peer_id: u64| -> u64 {
            peers
                .iter()
                .enumerate()
                .find_map(|(i, x)| {
                    if x.id == peer_id {
                        Some(i as u64)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| {
                    panic!("Peer {peer_id} must be present when collecting all peers.")
                })
        };

        let mut region_routes = Vec::with_capacity(self.region_routes.len());
        for region_route in self.region_routes.into_iter() {
            let leader_peer_index = region_route.leader_peer.map(|x| find_peer(x.id)).context(
                error::RouteInfoCorruptedSnafu {
                    err_msg: "'leader_peer' is empty in region route",
                },
            )?;

            let follower_peer_indexes = region_route
                .follower_peers
                .iter()
                .map(|x| find_peer(x.id))
                .collect::<Vec<_>>();

            region_routes.push(PbRegionRoute {
                region: Some(region_route.region.into()),
                leader_peer_index,
                follower_peer_indexes,
            });
        }

        let table_route = PbTableRoute {
            table: Some(self.table.into()),
            region_routes,
        };
        Ok((peers, table_route))
    }

    pub fn find_leaders(&self) -> HashSet<Peer> {
        self.region_routes
            .iter()
            .flat_map(|x| &x.leader_peer)
            .cloned()
            .collect()
    }

    pub fn find_leader_regions(&self, datanode: &Peer) -> Vec<u32> {
        self.region_routes
            .iter()
            .filter_map(|x| {
                if let Some(peer) = &x.leader_peer {
                    if peer == datanode {
                        return Some(x.region.id as u32);
                    }
                }
                None
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Table {
    pub id: u64,
    pub table_name: TableName,
    #[serde(serialize_with = "as_utf8")]
    pub table_schema: Vec<u8>,
}

impl TryFrom<PbTable> for Table {
    type Error = error::Error;

    fn try_from(t: PbTable) -> Result<Self> {
        let table_name = t
            .table_name
            .context(error::RouteInfoCorruptedSnafu {
                err_msg: "table name required",
            })?
            .into();
        Ok(Self {
            id: t.id,
            table_name,
            table_schema: t.table_schema,
        })
    }
}

impl From<Table> for PbTable {
    fn from(table: Table) -> Self {
        PbTable {
            id: table.id,
            table_name: Some(table.table_name.into()),
            table_schema: table.table_schema,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct RegionRoute {
    pub region: Region,
    pub leader_peer: Option<Peer>,
    pub follower_peers: Vec<Peer>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Region {
    pub id: u64,
    pub name: String,
    pub partition: Option<Partition>,
    pub attrs: HashMap<String, String>,
}

impl From<PbRegion> for Region {
    fn from(r: PbRegion) -> Self {
        Self {
            id: r.id,
            name: r.name,
            partition: r.partition.map(Into::into),
            attrs: r.attrs,
        }
    }
}

impl From<Region> for PbRegion {
    fn from(region: Region) -> Self {
        Self {
            id: region.id,
            name: region.name,
            partition: region.partition.map(Into::into),
            attrs: region.attrs,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Partition {
    #[serde(serialize_with = "as_utf8_vec")]
    pub column_list: Vec<Vec<u8>>,
    #[serde(serialize_with = "as_utf8_vec")]
    pub value_list: Vec<Vec<u8>>,
}

fn as_utf8<S: Serializer>(val: &[u8], serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(
        String::from_utf8(val.to_vec())
            .unwrap_or_else(|_| "<unknown-not-UTF8>".to_string())
            .as_str(),
    )
}

fn as_utf8_vec<S: Serializer>(
    val: &[Vec<u8>],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(
        val.iter()
            .map(|v| {
                String::from_utf8(v.clone()).unwrap_or_else(|_| "<unknown-not-UTF8>".to_string())
            })
            .collect::<Vec<String>>()
            .join(",")
            .as_str(),
    )
}

impl From<Partition> for PbPartition {
    fn from(p: Partition) -> Self {
        Self {
            column_list: p.column_list,
            value_list: p.value_list,
        }
    }
}

impl From<PbPartition> for Partition {
    fn from(p: PbPartition) -> Self {
        Self {
            column_list: p.column_list,
            value_list: p.value_list,
        }
    }
}

#[cfg(test)]
mod tests {
    use api::v1::meta::{
        DeleteRequest as PbDeleteRequest, Partition as PbPartition, Peer as PbPeer,
        Region as PbRegion, RegionRoute as PbRegionRoute, RouteRequest as PbRouteRequest,
        RouteResponse as PbRouteResponse, Table as PbTable, TableName as PbTableName,
        TableRoute as PbTableRoute,
    };
    use chrono::DateTime;
    use datatypes::prelude::ConcreteDataType;
    use datatypes::schema::{ColumnSchema, RawSchema};
    use table::metadata::{RawTableMeta, TableIdent, TableType};
    use table::requests::TableOptions;

    use super::*;

    fn new_table_info() -> RawTableInfo {
        RawTableInfo {
            ident: TableIdent {
                table_id: 0,
                version: 0,
            },
            name: "t1".to_string(),
            desc: None,
            catalog_name: "c1".to_string(),
            schema_name: "s1".to_string(),
            meta: RawTableMeta {
                schema: RawSchema {
                    column_schemas: vec![
                        ColumnSchema::new(
                            "ts",
                            ConcreteDataType::timestamp_millisecond_datatype(),
                            false,
                        ),
                        ColumnSchema::new("c1", ConcreteDataType::string_datatype(), true),
                        ColumnSchema::new("c2", ConcreteDataType::string_datatype(), true),
                    ],
                    timestamp_index: Some(0),
                    version: 0,
                },
                primary_key_indices: vec![],
                value_indices: vec![],
                engine: "mito".to_string(),
                next_column_id: 0,
                region_numbers: vec![],
                engine_options: HashMap::new(),
                options: TableOptions::default(),
                created_on: DateTime::default(),
            },
            table_type: TableType::Base,
        }
    }

    #[test]
    fn test_create_request_trans() {
        let req = CreateRequest {
            table_name: TableName::new("c1", "s1", "t1"),
            partitions: vec![
                Partition {
                    column_list: vec![b"c1".to_vec(), b"c2".to_vec()],
                    value_list: vec![b"v1".to_vec(), b"v2".to_vec()],
                },
                Partition {
                    column_list: vec![b"c1".to_vec(), b"c2".to_vec()],
                    value_list: vec![b"v11".to_vec(), b"v22".to_vec()],
                },
            ],
            table_info: &new_table_info(),
        };
        let into_req: PbCreateRequest = req.try_into().unwrap();

        assert!(into_req.header.is_none());
        let table_name = into_req.table_name;
        assert_eq!("c1", table_name.as_ref().unwrap().catalog_name);
        assert_eq!("s1", table_name.as_ref().unwrap().schema_name);
        assert_eq!("t1", table_name.as_ref().unwrap().table_name);
        assert_eq!(
            vec![b"c1".to_vec(), b"c2".to_vec()],
            into_req.partitions.get(0).unwrap().column_list
        );
        assert_eq!(
            vec![b"v1".to_vec(), b"v2".to_vec()],
            into_req.partitions.get(0).unwrap().value_list
        );
        assert_eq!(
            vec![b"c1".to_vec(), b"c2".to_vec()],
            into_req.partitions.get(1).unwrap().column_list
        );
        assert_eq!(
            vec![b"v11".to_vec(), b"v22".to_vec()],
            into_req.partitions.get(1).unwrap().value_list
        );
    }

    #[test]
    fn test_route_request_trans() {
        let req = RouteRequest {
            table_names: vec![
                TableName::new("c1", "s1", "t1"),
                TableName::new("c2", "s2", "t2"),
            ],
        };

        let into_req: PbRouteRequest = req.into();

        assert!(into_req.header.is_none());
        assert_eq!("c1", into_req.table_names.get(0).unwrap().catalog_name);
        assert_eq!("s1", into_req.table_names.get(0).unwrap().schema_name);
        assert_eq!("t1", into_req.table_names.get(0).unwrap().table_name);
        assert_eq!("c2", into_req.table_names.get(1).unwrap().catalog_name);
        assert_eq!("s2", into_req.table_names.get(1).unwrap().schema_name);
        assert_eq!("t2", into_req.table_names.get(1).unwrap().table_name);
    }

    #[test]
    fn test_delete_request_trans() {
        let req = DeleteRequest {
            table_name: TableName::new("c1", "s1", "t1"),
        };

        let into_req: PbDeleteRequest = req.into();

        assert!(into_req.header.is_none());
        assert_eq!("c1", into_req.table_name.as_ref().unwrap().catalog_name);
        assert_eq!("s1", into_req.table_name.as_ref().unwrap().schema_name);
        assert_eq!("t1", into_req.table_name.as_ref().unwrap().table_name);
    }

    #[test]
    fn test_route_response_trans() {
        let res = PbRouteResponse {
            header: None,
            peers: vec![
                PbPeer {
                    id: 1,
                    addr: "peer1".to_string(),
                },
                PbPeer {
                    id: 2,
                    addr: "peer2".to_string(),
                },
            ],
            table_routes: vec![PbTableRoute {
                table: Some(PbTable {
                    id: 1,
                    table_name: Some(PbTableName {
                        catalog_name: "c1".to_string(),
                        schema_name: "s1".to_string(),
                        table_name: "t1".to_string(),
                    }),
                    table_schema: b"schema".to_vec(),
                }),
                region_routes: vec![PbRegionRoute {
                    region: Some(PbRegion {
                        id: 1,
                        name: "region1".to_string(),
                        partition: Some(PbPartition {
                            column_list: vec![b"c1".to_vec(), b"c2".to_vec()],
                            value_list: vec![b"v1".to_vec(), b"v2".to_vec()],
                        }),
                        attrs: Default::default(),
                    }),
                    leader_peer_index: 0,
                    follower_peer_indexes: vec![1],
                }],
            }],
        };

        let res: RouteResponse = res.try_into().unwrap();
        let mut table_routes = res.table_routes;
        assert_eq!(1, table_routes.len());
        let table_route = table_routes.remove(0);
        let table = table_route.table;
        assert_eq!(1, table.id);
        assert_eq!("c1", table.table_name.catalog_name);
        assert_eq!("s1", table.table_name.schema_name);
        assert_eq!("t1", table.table_name.table_name);

        let mut region_routes = table_route.region_routes;
        assert_eq!(1, region_routes.len());
        let region_route = region_routes.remove(0);
        let region = region_route.region;
        assert_eq!(1, region.id);
        assert_eq!("region1", region.name);
        let partition = region.partition.unwrap();
        assert_eq!(vec![b"c1".to_vec(), b"c2".to_vec()], partition.column_list);
        assert_eq!(vec![b"v1".to_vec(), b"v2".to_vec()], partition.value_list);

        assert_eq!(1, region_route.leader_peer.as_ref().unwrap().id);
        assert_eq!("peer1", region_route.leader_peer.as_ref().unwrap().addr);

        assert_eq!(1, region_route.follower_peers.len());
        assert_eq!(2, region_route.follower_peers.get(0).unwrap().id);
        assert_eq!("peer2", region_route.follower_peers.get(0).unwrap().addr);
    }

    #[test]
    fn test_table_route_raw_conversion() {
        let raw_peers = vec![
            PbPeer {
                id: 1,
                addr: "a1".to_string(),
            },
            PbPeer {
                id: 2,
                addr: "a2".to_string(),
            },
            PbPeer {
                id: 3,
                addr: "a3".to_string(),
            },
        ];

        // region distribution:
        // region id => leader peer id + [follower peer id]
        // 1 => 2 + [1, 3]
        // 2 => 1 + [2, 3]

        let raw_table_route = PbTableRoute {
            table: Some(PbTable {
                id: 1,
                table_name: Some(PbTableName {
                    catalog_name: "c1".to_string(),
                    schema_name: "s1".to_string(),
                    table_name: "t1".to_string(),
                }),
                table_schema: vec![],
            }),
            region_routes: vec![
                PbRegionRoute {
                    region: Some(PbRegion {
                        id: 1,
                        name: "r1".to_string(),
                        partition: None,
                        attrs: HashMap::new(),
                    }),
                    leader_peer_index: 1,
                    follower_peer_indexes: vec![0, 2],
                },
                PbRegionRoute {
                    region: Some(PbRegion {
                        id: 2,
                        name: "r2".to_string(),
                        partition: None,
                        attrs: HashMap::new(),
                    }),
                    leader_peer_index: 0,
                    follower_peer_indexes: vec![1, 2],
                },
            ],
        };
        let table_route = TableRoute {
            table: Table {
                id: 1,
                table_name: TableName::new("c1", "s1", "t1"),
                table_schema: vec![],
            },
            region_routes: vec![
                RegionRoute {
                    region: Region {
                        id: 1,
                        name: "r1".to_string(),
                        partition: None,
                        attrs: HashMap::new(),
                    },
                    leader_peer: Some(Peer::new(2, "a2")),
                    follower_peers: vec![Peer::new(1, "a1"), Peer::new(3, "a3")],
                },
                RegionRoute {
                    region: Region {
                        id: 2,
                        name: "r2".to_string(),
                        partition: None,
                        attrs: HashMap::new(),
                    },
                    leader_peer: Some(Peer::new(1, "a1")),
                    follower_peers: vec![Peer::new(2, "a2"), Peer::new(3, "a3")],
                },
            ],
        };

        let from_raw = TableRoute::try_from_raw(&raw_peers, raw_table_route.clone()).unwrap();
        assert_eq!(from_raw, table_route);

        let into_raw = table_route.try_into_raw().unwrap();
        assert_eq!(into_raw.0, raw_peers);
        assert_eq!(into_raw.1, raw_table_route);
    }
}