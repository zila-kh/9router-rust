use base64::{engine::general_purpose::STANDARD,Engine};
use crate::error::AppError;

pub fn encode_frame(payload:&[u8],trailer:bool)->Vec<u8>{let mut out=Vec::with_capacity(payload.len()+5);out.push(if trailer{0x80}else{0});out.extend_from_slice(&(payload.len()as u32).to_be_bytes());out.extend_from_slice(payload);out}
pub fn decode_frames(input:&[u8],text_mode:bool)->Result<Vec<(bool,Vec<u8>)>,AppError>{let data=if text_mode{STANDARD.decode(input).map_err(|e|AppError::BadRequest(e.to_string()))?}else{input.to_vec()};let mut pos=0;let mut out=vec![];while pos<data.len(){if pos+5>data.len(){return Err(AppError::BadRequest("truncated grpc-web frame".into()))}let trailer=data[pos]&0x80!=0;let n=u32::from_be_bytes(data[pos+1..pos+5].try_into().unwrap())as usize;pos+=5;if pos+n>data.len(){return Err(AppError::BadRequest("truncated grpc-web payload".into()))}out.push((trailer,data[pos..pos+n].to_vec()));pos+=n}Ok(out)}
pub fn encode_text(frames:&[u8])->String{STANDARD.encode(frames)}

#[cfg(test)]
mod tests{use super::*;#[test]fn grpc_frames(){let mut wire=encode_frame(b"one",false);wire.extend(encode_frame(b"grpc-status: 0\r\n",true));let f=decode_frames(&wire,false).unwrap();assert_eq!(f[0],(false,b"one".to_vec()));assert!(f[1].0);let txt=encode_text(&wire);assert_eq!(decode_frames(txt.as_bytes(),true).unwrap(),f);}}
