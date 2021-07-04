// Copyright © 2016, Canal TP and/or its affiliates. All rights reserved.
//
// This file is part of Navitia,
//     the software to build cool stuff with public transport.
//
// Hope you'll enjoy and contribute to this project,
//     powered by Canal TP (www.canaltp.fr).
// Help us simplify mobility and open public transport:
//     a non ending quest to the responsive locomotion way of traveling!
//
// LICENCE: This program is free software; you can redistribute it
// and/or modify it under the terms of the GNU Affero General Public
// License as published by the Free Software Foundation, either
// version 3 of the License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
// Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public
// License along with this program. If not, see
// <http://www.gnu.org/licenses/>.
//
// Stay tuned using
// twitter @navitia
// IRC #navitia on freenode
// https://groups.google.com/d/forum/navitia
// www.navitia.io

use crate::admin_geofinder::AdminGeoFinder;
use crate::{labels, utils};
use failure::format_err;
use failure::Error;
use futures::stream::StreamExt;
use places::{admin::Admin, stop::Stop, MimirObject};
use serde::Serialize;

use mimir2::{
    adapters::secondary::elasticsearch::{
        self,
        internal::{IndexConfiguration, IndexMappings, IndexParameters, IndexSettings},
    },
    domain::model::{configuration::Configuration, document::Document, index::IndexVisibility},
    domain::ports::{list::ListParameters, remote::Remote},
    domain::usecases::list_documents::{ListDocuments, ListDocumentsParameters},
    domain::usecases::{
        generate_index::{GenerateIndex, GenerateIndexParameters},
        UseCase,
    },
};
use std::collections::HashMap;
use std::mem::replace;
use std::ops::Deref;
use std::sync::Arc;

// const GLOBAL_STOP_INDEX_NAME: &str = "munin_global_stops";

pub fn initialize_weights<'a, It, S: ::std::hash::BuildHasher>(
    stops: It,
    nb_stop_points: &HashMap<String, u32, S>,
) where
    It: Iterator<Item = &'a mut Stop>,
{
    let max = *nb_stop_points.values().max().unwrap_or(&1) as f64;
    for stop in stops {
        stop.weight = if let Some(weight) = nb_stop_points.get(&stop.id) {
            *weight as f64 / max
        } else {
            0.0
        };
    }
}

pub async fn import_stops(
    mut stops: Vec<Stop>,
    connection_string: &str,
    dataset: &str,
) -> Result<(), Error> {
    attach_stops_to_admins(stops.iter_mut(), connection_string).await?;

    // FIXME Should be done concurrently (for_each_concurrent....)
    for stop in &mut stops {
        stop.coverages.push(dataset.to_string());
        let mut admin_weight = stop
            .administrative_regions
            .iter()
            .filter(|adm| adm.is_city())
            .map(|adm| adm.weight)
            .next()
            .unwrap_or(0.0);
        // FIXME: 1024, automagic!
        // It's a factor used to bring the stop weight and the admin weight in the same order of
        // magnitude...
        // We then use a log to compress the distance between low admin weight and high ones.
        admin_weight = admin_weight * 1024.0 + 1.0;
        admin_weight = admin_weight.log10();
        stop.weight = (stop.weight + admin_weight) / 2.0;
    }

    let stops = futures::stream::iter(stops).map(StopDoc::from);

    let pool = elasticsearch::remote::connection_pool_url(&connection_string)
        .await
        .map_err(|err| {
            format_err!(
                "could not create elasticsearch connection pool: {}",
                err.to_string()
            )
        })?;

    let client = pool
        .conn()
        .await
        .map_err(|err| format_err!("could not connect elasticsearch pool: {}", err.to_string()))?;

    let config = IndexConfiguration {
        name: String::from(dataset),
        parameters: IndexParameters {
            timeout: String::from("10s"),
            wait_for_active_shards: String::from("1"), // only the primary shard
        },
        settings: IndexSettings {
            value: String::from(include_str!("../config/stop/settings.json")),
        },
        mappings: IndexMappings {
            value: String::from(include_str!("../config/stop/mappings.json")),
        },
    };

    let config = serde_json::to_string(&config).map_err(|err| {
        format_err!(
            "could not serialize index configuration: {}",
            err.to_string()
        )
    })?;
    let generate_index = GenerateIndex::new(Box::new(client));
    let parameters = GenerateIndexParameters {
        config: Configuration { value: config },
        documents: Box::new(stops),
        doc_type: String::from(StopDoc::DOC_TYPE),
        visibility: IndexVisibility::Public,
    };
    generate_index
        .execute(parameters)
        .await
        .map_err(|err| format_err!("could not generate index: {}", err.to_string()))?;

    // let global_index =
    //     update_global_stop_index(&mut rubber, stops.iter(), dataset, &index_settings)?;

    // info!("Importing {} stops into Mimir", stops.len());
    // let nb_stops = rubber.public_index(dataset, &index_settings, stops.into_iter())?;
    // info!("Nb of indexed stops: {}", nb_stops);

    // publish_global_index(&mut rubber, &global_index)
    //     .context("Error while publishing global index")?;
    Ok(())
}

fn attach_stop(stop: &mut Stop, admins: Vec<Arc<Admin>>) {
    let admins_iter = admins.iter().map(|a| a.deref());
    let country_codes = utils::find_country_codes(admins_iter.clone());

    stop.label = labels::format_stop_label(&stop.name, admins_iter, &country_codes);
    stop.zip_codes = utils::get_zip_codes_from_admins(&admins);

    stop.country_codes = country_codes;
    stop.administrative_regions = admins;
}

/// Attach the stops to administrative regions
///
/// The admins are loaded from Elasticsearch and stored in a quadtree
/// We attach a stop with all the admins that have a boundary containing
/// the coordinate of the stop
/// FIXME Use Stream instead of Iterator.
async fn attach_stops_to_admins<'a, It: Iterator<Item = &'a mut Stop>>(
    stops: It,
    connection_string: &str,
) -> Result<u32, crate::Error> {
    let pool = elasticsearch::remote::connection_pool_url(&connection_string)
        .await
        .map_err(|err| {
            format_err!(
                "could not create elasticsearch connection pool: {}",
                err.to_string()
            )
        })?;

    let client = pool
        .conn()
        .await
        .map_err(|err| format_err!("could not connect elasticsearch pool: {}", err.to_string()))?;

    let list_documents = ListDocuments::new(Box::new(client.clone()));

    let parameters = ListDocumentsParameters {
        parameters: ListParameters {
            doc_type: String::from(Admin::doc_type()),
        },
    };

    let admin_stream = list_documents
        .execute(parameters)
        .await
        .map_err(|err| format_err!("could not retrieve admins: {}", err.to_string()))?;

    let admins = admin_stream
        .map(|v| serde_json::from_value(v).expect("cannot deserialize admin"))
        .collect::<Vec<Admin>>()
        .await;

    let admins_geofinder = admins.into_iter().collect::<AdminGeoFinder>();

    let mut nb_unmatched = 0u32;
    let mut nb_matched = 0u32;
    // FIXME Opportunity for concurrent work
    for mut stop in stops {
        let admins = admins_geofinder.get(&stop.coord);

        if admins.is_empty() {
            nb_unmatched += 1;
        } else {
            nb_matched += 1;
        }

        attach_stop(&mut stop, admins);
    }

    Ok(nb_matched)
    // info!(
    //     "there are {}/{} stops without any admin",
    //     nb_unmatched,
    //     nb_matched + nb_unmatched
    // );
}

fn merge_collection<T: Ord>(target: &mut Vec<T>, source: Vec<T>) {
    use std::collections::BTreeSet;
    let tmp = replace(target, vec![]);
    *target = tmp
        .into_iter()
        .chain(source)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
}

/// merge the stops from all the different indexes
/// for the moment the merge is very simple and uses only the ID
/// (and we take the data from the first stop inserted)
fn merge_stops<It: IntoIterator<Item = Stop>>(stops: It) -> impl Iterator<Item = Stop> {
    let mut stops_by_id = HashMap::<String, Stop>::new();
    for mut stop in stops.into_iter() {
        let cov = replace(&mut stop.coverages, vec![]);
        let codes = replace(&mut stop.codes, vec![]);
        let physical_modes = replace(&mut stop.physical_modes, vec![]);
        let commercial_modes = replace(&mut stop.commercial_modes, vec![]);
        let properties = replace(&mut stop.properties, vec![]);
        let feed_publishers = replace(&mut stop.feed_publishers, vec![]);

        let stop_in_map = stops_by_id.entry(stop.id.clone()).or_insert(stop);

        merge_collection(&mut stop_in_map.codes, codes);
        merge_collection(&mut stop_in_map.physical_modes, physical_modes);
        merge_collection(&mut stop_in_map.commercial_modes, commercial_modes);
        merge_collection(&mut stop_in_map.coverages, cov);
        merge_collection(&mut stop_in_map.properties, properties);
        merge_collection(&mut stop_in_map.feed_publishers, feed_publishers);
    }
    stops_by_id.into_iter().map(|(_, v)| v)
}

// fn get_all_stops(rubber: &mut Rubber, index: String) -> Result<Vec<Stop>, Error> {
//     rubber
//         .get_all_objects_from_index(&index)
//         .map_err(|e| format_err!("Getting all stops {}", e.to_string()))
// }

// fn update_global_stop_index<'a, It: Iterator<Item = &'a Stop>>(
//     rubber: &mut Rubber,
//     stops: It,
//     dataset: &str,
//     index_settings: &IndexSettings,
// ) -> Result<String, Error> {
//     let dataset_index = mimir::rubber::get_main_type_and_dataset_index::<Stop>(dataset);
//     let stops_indexes = rubber
//         .get_all_aliased_index(&mimir::rubber::get_main_type_index::<Stop>())?
//         .into_iter()
//         .filter(|&(_, ref aliases)| !aliases.contains(&dataset_index))
//         .map(|(index, _)| index);
//
//     let all_es_stops = stops_indexes
//         .map(|index| get_all_stops(rubber, index))
//         .collect::<Result<Vec<_>, _>>()?
//         .into_iter()
//         .flat_map(|stops| stops.into_iter())
//         .chain(stops.cloned());
//
//     let all_merged_stops = merge_stops(all_es_stops);
//     let es_index_name = mimir::rubber::get_date_index_name(GLOBAL_STOP_INDEX_NAME);
//
//     rubber.create_index(&es_index_name, &index_settings)?;
//     let typed_index = TypedIndex::new(es_index_name.clone());
//
//     let nb_stops_added = rubber.bulk_index(&typed_index, all_merged_stops)?;
//     info!("{} stops added in the global index", nb_stops_added);
//     // create global index
//     // fill structure for each stop indexes
//     Ok(es_index_name)
// }
//
// publish the global stop index
// alias the new index to the global stop alias, and remove the old index
// fn publish_global_index(rubber: &mut Rubber, new_global_index: &str) -> Result<(), Error> {
//     let last_global_indexes: Vec<_> = rubber
//         .get_all_aliased_index(GLOBAL_STOP_INDEX_NAME)?
//         .into_iter()
//         .map(|(k, _)| k)
//         .filter(|k| k != new_global_index)
//         .collect();
//     rubber.alias(
//         GLOBAL_STOP_INDEX_NAME,
//         &[new_global_index.to_string()],
//         &last_global_indexes,
//     )?;
//
//     for index in last_global_indexes {
//         rubber.delete_index(&index)?;
//     }
//     Ok(())
// }

// We use a new type to wrap around Stop and implement the Document trait.
#[derive(Serialize)]
pub struct StopDoc(Stop);

impl Document for StopDoc {
    fn doc_type(&self) -> &'static str {
        Self::DOC_TYPE
    }
    fn id(&self) -> String {
        self.0.id.clone()
    }
}

impl StopDoc {
    const DOC_TYPE: &'static str = "stop";
}

impl From<Stop> for StopDoc {
    fn from(stop: Stop) -> Self {
        StopDoc(stop)
    }
}
