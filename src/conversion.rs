use std::convert::Infallible;

use k8s_openapi::chrono::Utc;
use kube::client::Status;
use kube::core::conversion::{ConversionRequest, ConversionResponse, ConversionReview};
use serde_json::{json, Value};
use warp::{Reply, reply};

pub async fn crdconvert_handler(body: ConversionReview) -> Result<impl Reply, Infallible> {
    let req: ConversionRequest = match body.try_into() {
        Ok(req) => req,
        Err(err) => {
            error!("Invalid CRD conversion request: {}", err.to_string());
            return Ok(reply::json(
                &ConversionResponse::invalid(Status::failure(err.to_string().as_ref(), "ConversionRequestInvalid")).into_review(),
            ));
        }
    };
    let conversion_response = convert_all(req).await;
    Ok(reply::json(&conversion_response.into_review()))
}

async fn convert_all(req: ConversionRequest) -> ConversionResponse {
    let desired_api_version = &req.desired_api_version.clone();
    let mut converted_objects: Vec<Value> = Vec::new();
    let objects_to_convert = req.objects.clone();
    let res = ConversionResponse::from(req);

    for obj in objects_to_convert {
        let converted_object = match convert(obj, desired_api_version) {
            Ok(converted_value) => {
                converted_value
            }
            Err(err) => {
                return res.failure(Status::failure(err.message.as_ref(), "ConversionFailure"));
            }
        };
        converted_objects.push(converted_object);
    }
    res.success(converted_objects)
}

fn convert(mut request_obj: Value, desired_api_version: &str) -> Result<Value, Status> {
    let current_api_version: String = request_obj.as_object()
        .map(|x| x.get("apiVersion")).flatten()
        .map(|x| x.as_str()).flatten()
        .ok_or(Status::failure("No apiVersion in request", "ConversionRequestInvalid"))?
        .to_string();

    if current_api_version == desired_api_version {
        return Err(Status::failure(format!("Already at desired API version {}", desired_api_version).as_ref(), "AlreadyAtDesiredAPIVersion"));
    }
    let orig_req_obj = request_obj.to_string();
    match desired_api_version {
        "tomjo.net/v1" => {
            match current_api_version.as_str() {
                "tomjo.net/v2" => {
                    request_obj["apiVersion"] = Value::String("tomjo.net/v1".to_string());
                    request_obj["status"]["start_time"] = request_obj["status"]["startTime"].clone();
                    request_obj["status"]["phase"] = Value::String("Succeeded".to_string());
                    request_obj["status"].as_object_mut().and_then(|m| m.remove("observedGeneration"));
                    request_obj["status"].as_object_mut().and_then(|m| m.remove("startTime"));

                    if let Some(conditions) = request_obj["status"]["conditions"].as_array_mut() {
                        for condition in conditions {
                            if let Some(condition_type) = condition["type"].as_str() {
                                if condition_type == "Ready" {
                                    let status = &condition["status"];
                                    if status.as_str() == Some("True") {
                                        condition["status"] = Value::Bool(true);
                                    } else if status.as_str() == Some("False") {
                                        condition["status"] = Value::Bool(false);
                                    } else {
                                        condition["status"] = Value::Bool(true);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    trace!("converting {} to {} | req: {} resp: {}", current_api_version, desired_api_version, orig_req_obj , request_obj);
                    Ok(request_obj)
                }
                _ => Err(Status::failure(format!("Unsupported API conversion from {} to {}", current_api_version, desired_api_version).as_ref(), "UnsupportedAPIConversion")),
            }
        }
        "tomjo.net/v2" => {
            match current_api_version.as_str() {
                "tomjo.net/v1" => {
                    request_obj["apiVersion"] = Value::String("tomjo.net/v2".to_string());
                    let orig_start_time = request_obj["status"]["start_time"].clone();
                    request_obj["status"]["startTime"] = if orig_start_time.is_null() { Value::Null } else { Value::String(Utc::now().to_rfc3339()) };
                    request_obj["status"].as_object_mut().and_then(|m| m.remove("phase"));
                    request_obj["status"].as_object_mut().and_then(|m| m.remove("start_time"));
                    request_obj["status"]["observedGeneration"] = json!(0);

                    if let Some(conditions) = request_obj["status"]["conditions"].as_array_mut() {
                        for condition in conditions {
                            if let Some(condition_type) = condition["type"].as_str() {
                                if condition_type == "Ready" {
                                    let status = &condition["status"];
                                    if status.as_bool() == Some(true) {
                                        condition["status"] = Value::String("True".to_string());
                                    } else if status.as_bool() == Some(false) {
                                        condition["status"] = Value::String("False".to_string());
                                    } else {
                                        condition["status"] = Value::String("Unknown".to_string());
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    trace!("converting {} to {} | req: {} resp: {}", current_api_version, desired_api_version, orig_req_obj , request_obj);
                    Ok(request_obj)
                }
                _ => Err(Status::failure(format!("Unsupported API conversion from {} to {}", current_api_version, desired_api_version).as_ref(), "UnsupportedAPIConversion")),
            }
        }
        _ => Err(Status::failure(format!("Unsupported API version {}", desired_api_version).as_ref(), "UnsupportedAPIVersion")),
    }
}
