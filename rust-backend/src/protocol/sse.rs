#[derive(Debug,Clone,Default,PartialEq,Eq)]
pub struct Event{pub event:Option<String>,pub data:String,pub id:Option<String>}
#[derive(Default)]pub struct Decoder{buf:String}
impl Decoder{pub fn push(&mut self,chunk:&str)->Vec<Event>{self.buf.push_str(chunk);let mut out=vec![];loop{let split=self.buf.find("\n\n").or_else(||self.buf.find("\r\n\r\n"));let Some(i)=split else{break};let sep=if self.buf[i..].starts_with("\r\n\r\n"){4}else{2};let raw=self.buf[..i].replace("\r\n","\n");self.buf.drain(..i+sep);let mut e=Event::default();let mut data=vec![];for l in raw.lines(){if l.starts_with(':'){continue}let(mut k,mut v)=l.split_once(':').unwrap_or((l,""));if v.starts_with(' '){v=&v[1..]}match k{"event"=>e.event=Some(v.into()),"data"=>data.push(v),"id"=>e.id=Some(v.into()),_=>{k="";let _=k;}}}e.data=data.join("\n");out.push(e)}out}}

#[cfg(test)]
mod tests{use super::*;#[test]fn incremental(){let mut d=Decoder::default();assert!(d.push("event: x\ndata: hel").is_empty());let e=d.push("lo\ndata: world\n\n");assert_eq!(e.len(),1);assert_eq!(e[0].event.as_deref(),Some("x"));assert_eq!(e[0].data,"hello\nworld");}}
