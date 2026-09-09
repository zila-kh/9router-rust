use crate::error::AppError;

pub fn put_varint(mut v:u64,out:&mut Vec<u8>){while v>=0x80{out.push((v as u8)|0x80);v>>=7}out.push(v as u8)}
pub fn get_varint(input:&[u8],pos:&mut usize)->Result<u64,AppError>{let mut v=0u64;for shift in (0..70).step_by(7){if *pos>=input.len(){return Err(AppError::BadRequest("truncated protobuf varint".into()))}let b=input[*pos];*pos+=1;v|=((b&0x7f)as u64)<<shift;if b&0x80==0{return Ok(v)}}Err(AppError::BadRequest("protobuf varint overflow".into()))}
pub fn key(field:u32,wire:u8,out:&mut Vec<u8>){put_varint(((field as u64)<<3)|wire as u64,out)}
pub fn put_bytes(field:u32,b:&[u8],out:&mut Vec<u8>){key(field,2,out);put_varint(b.len()as u64,out);out.extend_from_slice(b)}
pub fn put_string(field:u32,s:&str,out:&mut Vec<u8>){put_bytes(field,s.as_bytes(),out)}
pub fn put_u64(field:u32,v:u64,out:&mut Vec<u8>){key(field,0,out);put_varint(v,out)}

#[derive(Debug,Clone,PartialEq)]
pub enum FieldValue{Varint(u64),Fixed64(u64),Bytes(Vec<u8>),Fixed32(u32)}
#[derive(Debug,Clone,PartialEq)]
pub struct Field{pub number:u32,pub value:FieldValue}
pub fn decode_fields(input:&[u8])->Result<Vec<Field>,AppError>{let mut pos=0;let mut out=vec![];while pos<input.len(){let k=get_varint(input,&mut pos)?;let number=(k>>3)as u32;let wire=(k&7)as u8;let value=match wire{0=>FieldValue::Varint(get_varint(input,&mut pos)?),1=>{if pos+8>input.len(){return Err(AppError::BadRequest("truncated fixed64".into()))}let v=u64::from_le_bytes(input[pos..pos+8].try_into().unwrap());pos+=8;FieldValue::Fixed64(v)},2=>{let n=get_varint(input,&mut pos)?as usize;if pos+n>input.len(){return Err(AppError::BadRequest("truncated length field".into()))}let b=input[pos..pos+n].to_vec();pos+=n;FieldValue::Bytes(b)},5=>{if pos+4>input.len(){return Err(AppError::BadRequest("truncated fixed32".into()))}let v=u32::from_le_bytes(input[pos..pos+4].try_into().unwrap());pos+=4;FieldValue::Fixed32(v)},_=>return Err(AppError::BadRequest(format!("unsupported protobuf wire type {wire}")))};out.push(Field{number,value})}Ok(out)}

pub fn connect_envelope(message:&[u8],compressed:bool)->Vec<u8>{let mut out=Vec::with_capacity(message.len()+5);out.push(if compressed{1}else{0});out.extend_from_slice(&(message.len()as u32).to_be_bytes());out.extend_from_slice(message);out}
pub fn decode_connect_frames(mut input:&[u8])->Result<Vec<(u8,Vec<u8>)>,AppError>{let mut out=vec![];while !input.is_empty(){if input.len()<5{return Err(AppError::BadRequest("truncated ConnectRPC envelope".into()))}let flag=input[0];let n=u32::from_be_bytes(input[1..5].try_into().unwrap())as usize;if input.len()<5+n{return Err(AppError::BadRequest("truncated ConnectRPC message".into()))}out.push((flag,input[5..5+n].to_vec()));input=&input[5+n..]}Ok(out)}

#[cfg(test)]
mod tests{
    use super::*;
    #[test] fn protobuf_fields_round_trip(){let mut b=vec![];put_u64(1,150,&mut b);put_string(2,"hello",&mut b);let f=decode_fields(&b).unwrap();assert_eq!(f[0],Field{number:1,value:FieldValue::Varint(150)});assert_eq!(f[1],Field{number:2,value:FieldValue::Bytes(b"hello".to_vec())});}
    #[test] fn connect_round_trip(){let f=connect_envelope(b"abc",false);assert_eq!(decode_connect_frames(&f).unwrap(),vec![(0,b"abc".to_vec())]);}
}
