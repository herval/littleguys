import { test } from "node:test";
import assert from "node:assert/strict";
import { blank, reduceThread, waitingCount } from "../src/stores/chatReducer.ts";
import { createChatStore } from "../src/stores/chat.ts";
import { decodeChatEvent } from "../src/lib/chatEvents.ts";
import { toItems } from "../src/lib/transcript.ts";
const m = (id, role = "assistant", runId, clientSendId) => ({role, content:id, timestamp:1, __openclaw:{id, runId, clientSendId}});
const event = (t, e) => reduceThread(t, {type:"event",event:e,now:2});
const run = (t, runId, state) => event(t,{kind:"run", runId,state});
const send = (t, id) => reduceThread(t,{type:"send",pending:{idempotencyKey:id,status:"sending",message:m(id,"user")}});
const receipt = (t,id,runId) => reduceThread(t,{type:"receipt",id,receipt:{runId,status:"started"}});
const history = (t,messages,request=t.request,epoch=t.epoch,inFlightRun) => reduceThread(t,{type:"history",history:{sessionKey:"s",messages,inFlightRun},request,epoch,now:3});
const deferred = () => { let resolve, reject; const promise = new Promise((a,b)=>{resolve=a;reject=b}); return {promise,resolve,reject}; };

test("history cannot erase messages arriving while it loads",()=>{
 let t=reduceThread(blank("g|s"),{type:"load",request:1});
 t=event(t,{kind:"message",message:m("new")});
 t=history(t,[m("old")]);
 assert.deepEqual(t.messages.map(m=>m.content),["old","new"]);
 t=history(t,[m("old"),m("new")]);
 assert.equal(t.messages.length,2);
 assert.equal(t.observed.length,0);
});
test("older history requests and disconnected epochs are ignored",()=>{
 let t=reduceThread(blank("g|s"),{type:"load",request:2});
 assert.equal(history(t,[m("old")],1),t);
 t=reduceThread(t,{type:"disconnect"});
 assert.equal(history(t,[m("old")],2,0),t);
});
test("final before send response or history never resurrects a run",()=>{
 let t=send(blank("g|s"),"send");
 t=run(t,"r","final");
 t=receipt(t,"send","r");
 t=history(t,[],0,0,{runId:"r"});
 assert.equal(t.live,null);
 assert.equal(run(t,"r","status").live,null);
});
test("a stale final cannot clear a newer run or its error state",()=>{
 let t=run(blank("g|s"),"old","status");
 t=run(t,"new","status");
 t=run(t,"old","error");
 assert.equal(t.live.runId,"new");
 assert.equal(t.sendError,null);
 assert.equal(run(t,"old","delta").live.runId,"new");
});
test("terminal and phone messages do not acknowledge an unrelated send",()=>{
 let t=receipt(send(blank("g|s"),"send"),"send","r");
 t=event(t,{kind:"message",message:m("terminal","user","external")});
 assert.equal(t.pending.length,1);
 t=event(t,{kind:"message",message:m("ours","user","r")});
 assert.equal(t.pending.length,0);
});
test("a message before its RPC response is reconciled by run ID",()=>{
 let t=send(blank("g|s"),"send");
 t=event(t,{kind:"message",message:m("ours","user","r")});
 t=receipt(t,"send","r");
 assert.equal(t.pending.length,0);
});
test("delivery acknowledgements retain the right queued prompts until transcript confirmation",()=>{
 let t=send(send(blank("g|s"),"a"),"b");
 t=event(t,{kind:"delivery",ids:["b"],status:"delivered"});
 assert.deepEqual(t.pending.map(p=>p.idempotencyKey),["a","b"]);
 assert.equal(t.pending[0].delivery,undefined);
 assert.deepEqual(t.pending[1].delivery.ids,["b"]);
 t=receipt(t,"b","r");
 assert.equal(t.pending.length,2);
 t=event(t,{kind:"message",message:m("saved-b","user","r","b")});
 assert.deepEqual(t.pending.map(p=>p.idempotencyKey),["a"]);
});
test("duplicate transcript events and tool starts are idempotent",()=>{
 let t=blank("g|s");
 for(let i=0;i<2;i++) t=event(t,{kind:"message",message:m("same")});
 t=event(t,{kind:"tool",id:"tool",status:"completed"});
 t=event(t,{kind:"tool",id:"tool",status:"running"});
 assert.equal(t.messages.length,1);
 assert.equal(t.toolStatus.tool,"completed");
});
test("abort ends only its matching run",()=>{
 let t=run(blank("g|s"),"r","status");
 t=run(t,"r","aborted");
 assert.equal(t.live,null);
 assert.equal(run(t,"r","delta").live,null);
});
test("history acknowledges only correlated pending messages",()=>{
 let t=receipt(send(send(blank("g|s"),"a"),"b"),"a","r");
 t=history(t,[m("ours","user","r"),m("external","user")]);
 assert.deepEqual(t.pending.map(p=>p.idempotencyKey),["b"]);
 assert.equal(waitingCount(t),1);
});
test("malformed and unknown wire events are ignored; legacy messages still work",()=>{
 assert.equal(decodeChatEvent("chat",{state:"final"}),null);
 assert.equal(decodeChatEvent("session.message",{message:{role:"user"}}),null);
 assert.equal(decodeChatEvent("future",{}),null);
 assert.equal(decodeChatEvent("session.message",{message:m("legacy")}).kind,"message");
});
test("store passes the optimistic ID through to transport",async()=>{
 let args;
 const sendReply=deferred();
 const store=createChatStore({history:async()=>({messages:[]}),send:(...a)=>{args=a;return sendReply.promise},abort:async()=>{}},async()=>{});
 const p=store.getState().send("g|s","hello",[]);
 const id=store.getState().threads["g|s"].pending[0].idempotencyKey;
 assert.equal(args[5],id);
 store.getState().handleEvent("g","session.message",{sessionKey:"s",clientSendId:id,message:m("saved","user")});
 sendReply.resolve({runId:"r",status:"started"});
 await p;
 assert.equal(store.getState().threads["g|s"].pending.length,0);
});
test("store rejects out-of-order history and reloads on reconnect without resending",async()=>{
 const a=deferred(),b=deferred(); let n=0, sends=0, subscriptions=0;
 const store=createChatStore({history:()=>++n===1?a.promise:b.promise,send:async()=>{sends++;},abort:async()=>{}},async()=>{subscriptions++;});
 const one=store.getState().reload("g|s"),two=store.getState().reload("g|s");
 b.resolve({messages:[m("new")]}); await two;
 a.resolve({messages:[m("old")]}); await one;
 assert.equal(store.getState().threads["g|s"].messages[0].content,"new");
 store.getState().disconnect("g");store.getState().resubscribe("g");
 await Promise.resolve();
 assert.equal(subscriptions,1);assert.equal(sends,0);
});
test("ambiguous send failure remains visible and is never retried",async()=>{
 let sends=0;
 const store=createChatStore({history:async()=>({messages:[]}),send:async()=>{sends++;throw {code:"TIMEOUT",message:"timeout"}},abort:async()=>{throw Error("offline")}},async()=>{});
 await store.getState().send("g|s","hello",[]);
 assert.equal(sends,1);assert.equal(store.getState().threads["g|s"].pending[0].status,"unknown");
 await store.getState().abort("g|s");
 assert.match(store.getState().threads["g|s"].sendError,/Couldn't stop/);
});
test("definite rejection restores the composer without leaving optimistic text",async()=>{
 const store=createChatStore({history:async()=>({messages:[]}),send:async()=>{throw {code:"INVALID_REQUEST",message:"bad"}},abort:async()=>{}},async()=>{});
 await assert.rejects(store.getState().send("g|s","hello",[]));
 assert.equal(store.getState().threads["g|s"].pending.length,0);
});

test("an explicit client ID takes precedence over a shared runtime run ID",()=>{
 let t=receipt(send(blank("g|s"),"a"),"a","shared");
 t=event(t,{kind:"message",message:m("first","user","shared","a")});
 t=receipt(send(t,"b"),"b","shared");
 assert.equal(t.pending.length,1);
 t=event(t,{kind:"message",message:m("external","user","shared","phone")});
 assert.equal(t.pending.length,1);
 t=event(t,{kind:"message",message:m("second","user","shared","b")});
 assert.equal(t.pending.length,0);
});

test("shared Rust/frontend wire fixture settles without duplicate optimistic messages",async()=>{
 const {readFile}=await import("node:fs/promises");
 const fixture=JSON.parse(await readFile(new URL("./fixtures/chat-contract.json",import.meta.url),"utf8"));
 let t=send(blank("g|agent:a:main"),"send-1");
 for (const frame of fixture.events) {
   const decoded=decodeChatEvent(frame.event,frame.payload);
   if(decoded) t=event(t,decoded);
 }
 t=reduceThread(t,{type:"receipt",id:"send-1",receipt:fixture.receipt});
 assert.equal(t.pending.length,0);assert.equal(t.live,null);assert.equal(t.messages.length,1);
});

test("replayed or out-of-order sequenced deltas do not repeat streamed text",()=>{
 let t=blank("g|s");
 t=event(t,{kind:"run",runId:"r",state:"delta",deltaText:"Hello",seq:2});
 t=event(t,{kind:"run",runId:"r",state:"delta",deltaText:"Hello",seq:2});
 t=event(t,{kind:"run",runId:"r",state:"delta",deltaText:"old",seq:1});
 assert.equal(t.live.text,"Hello");
});
test("a late RPC timeout cannot undo an explicit delivery acknowledgement",()=>{
 let t=send(blank("g|s"),"a");
 t=event(t,{kind:"delivery",ids:["a"],status:"delivered"});
 t=reduceThread(t,{type:"sendError",id:"a",uncertain:true,error:"timeout"});
 assert.equal(t.sendError,null);assert.equal(t.pending.length,1);
 assert.ok(t.pending[0].delivery);
 t=event(t,{kind:"delivery",ids:["from-phone"],status:"unknown",error:"external failure"});
 assert.equal(t.sendError,null);
});


test("a late assistant transcript cannot create a phantom live run",()=>{
 let t=run(blank("g|s"),"r","status");
 t=run(t,"r","final");
 t=event(t,{kind:"message",message:m("late")});
 assert.equal(t.live,null);
 t=reduceThread(t,{type:"load",request:1});
 t=history(t,[m("late")]);
 assert.equal(t.live,null);
 t=event(t,{kind:"message",message:m("another")});
 assert.equal(t.live,null);
});

test("settled Hermes live segments are replaced by canonical history with different IDs",()=>{
 let t=run(blank("g|s"),"hermes-r","status");
 for(const id of ["live:hermes-r:user","live:hermes-r:1","live:hermes-r:r:tool","live:hermes-r:2"]){
   t=event(t,{kind:"message",runId:"hermes-r",message:m(id)});
 }
 t=run(t,"hermes-r","final");
 t=reduceThread(t,{type:"load",request:1});
 // This newer turn started while the snapshot was in flight; keep its event.
 t=run(t,"next","status");
 t=event(t,{kind:"message",runId:"next",message:m("live:next:user","user")});
 t=history(t,[m("h:1","user"),m("h:2"),m("h:3","toolResult"),m("h:4")]);
 assert.deepEqual(t.messages.map(m=>m.__openclaw.id),["h:1","h:2","h:3","h:4","live:next:user"]);
 assert.equal(t.observed.length,1);
 assert.equal(t.live.runId,"next");
});

test("a history request begun during a Hermes turn retains its unwritten live segments",()=>{
 let t=run(blank("g|s"),"r","status");
 t=event(t,{kind:"message",runId:"r",message:m("live:r:1")});
 t=reduceThread(t,{type:"load",request:1});
 t=run(t,"r","final");
 t=history(t,[]);
 assert.equal(t.messages.length,1);
 t=reduceThread(t,{type:"load",request:2});
 t=history(t,[m("h:1")]);
 assert.deepEqual(t.messages.map(m=>m.__openclaw.id),["h:1"]);
});

const nativeSend=(t,id,text="hello",timestamp=2001)=>reduceThread(t,{type:"send",pending:{
 idempotencyKey:id,status:"sending",message:{role:"user",content:text,timestamp}
}});
const nativeMessage=(id,text="hello",timestamp=2002)=>({role:"user",content:[{type:"text",text}],timestamp,__openclaw:{id}});

test("native queued prompts stay visible across reloads and disconnect until their transcript arrives",()=>{
 let t=nativeSend(blank("g|s"),"send");
 t=receipt(t,"send","receipt-id");
 t=event(t,{kind:"delivery",ids:["send"],status:"delivered"});
 assert.equal(t.pending.length,1);
 assert.equal(waitingCount(t),1);
 t=reduceThread(t,{type:"disconnect"});
 t=reduceThread(t,{type:"load",request:1});
 t=history(t,[]);
 assert.equal(t.pending[0].status,"queued");
 t=history(t,[nativeMessage("native-row")]);
 assert.equal(t.pending.length,0);
 assert.equal(t.messages.length,1);
});

test("native transcript can arrive before its delivery acknowledgement",()=>{
 let t=nativeSend(blank("g|s"),"send");
 t=event(t,{kind:"message",message:nativeMessage("native-row")});
 assert.equal(t.pending.length,1);
 t=event(t,{kind:"delivery",ids:["send"],status:"delivered"});
 assert.equal(t.pending.length,0);
 assert.equal(t.messages.length,1);
});

test("accepted native batches reconcile together without claiming unrelated or old transcript rows",()=>{
 let t=history(blank("g|s"),[nativeMessage("already-there","one\n\ntwo")]);
 t=nativeSend(nativeSend(t,"a","one"),"b","two");
 t=event(t,{kind:"delivery",ids:["a","b"],status:"delivered",text:"one\n\ntwo"});
 assert.equal(t.pending.length,2);
 for(const message of [nativeMessage("old","one\n\ntwo",1000),nativeMessage("terminal","different"),
  {...nativeMessage("phone","one\n\ntwo"),__openclaw:{id:"phone",clientSendId:"external"}}]) {
   t=event(t,{kind:"message",message});
   assert.equal(t.pending.length,2);
 }
 t=event(t,{kind:"message",message:nativeMessage("ours","one\n\ntwo")});
 assert.equal(t.pending.length,0);
});

test("one native transcript row cannot confirm two identical handoffs, even on replay",()=>{
 let t=nativeSend(nativeSend(blank("g|s"),"a"),"b");
 for(const id of ["a","b"]) t=event(t,{kind:"delivery",ids:[id],status:"delivered"});
 for(let i=0;i<2;i++) t=event(t,{kind:"message",message:nativeMessage("first")});
 t=history(t,[nativeMessage("first")]);
 assert.deepEqual(t.pending.map(p=>p.idempotencyKey),["b"]);
 t=event(t,{kind:"message",message:nativeMessage("second")});
 assert.equal(t.pending.length,0);
});

test("delivery raw text reconciles native attachment and hook wrappers",()=>{
 let t=nativeSend(blank("g|s"),"a","look");
 const text="look\n\n[attached image: /tmp/photo.png]";
 const decoded=decodeChatEvent("chat.delivery",{clientSendIds:["a"],status:"delivered",text});
 t=event(t,decoded);
 assert.equal(t.pending.length,1);
 t=event(t,{kind:"message",message:nativeMessage("attachment",text)});
 assert.equal(t.pending.length,0);
});
const queued = (id, text = id) => ({id, text, attachments: [], at: 1});
test("a message littleguys holds leaves the thread and waits over the composer",async()=>{
 let args;
 const store=createChatStore({history:async()=>({messages:[],queued:[queued("q0")]}),send:async(...a)=>{args=a;return {runId:"q1",status:"held"}},abort:async()=>{},unqueue:async()=>{}},async()=>{});
 await store.getState().open("g|s");
 assert.deepEqual(store.getState().threads["g|s"].queued.map(q=>q.id),["q0"]);
 await store.getState().send("g|s","later",[],undefined,false,"queue");
 assert.equal(args[7],"queue");
 assert.equal(store.getState().threads["g|s"].pending.length,0);
 store.getState().handleEvent("g","chat.queue",{sessionKey:"s",queued:[queued("q0"),queued("q1","later")]});
 assert.deepEqual(store.getState().threads["g|s"].queued.map(q=>q.text),["q0","later"]);
 await store.getState().unqueue("g|s","q0");
 assert.deepEqual(store.getState().threads["g|s"].queued.map(q=>q.id),["q1"]);
});
test("what littleguys hands over shows as sending until the thread has it",()=>{
 let t=event(blank("g|s"),{kind:"queue",queued:[queued("a"),queued("b")]});
 t=event(t,{kind:"queue",queued:[],sent:{id:"batch",text:"a\n\nb"}});
 assert.equal(t.queued.length,0);
 assert.equal(t.pending.length,1);
 assert.equal(t.pending[0].idempotencyKey,"batch");
 t=event(t,{kind:"queue",queued:[],accepted:{id:"batch",receipt:{runId:"r",status:"started"}}});
 assert.equal(t.pending[0].runId,"r");
 t=event(t,{kind:"message",message:m("a\n\nb","user","r","batch")});
 assert.equal(t.pending.length,0);
 // The same hand-over seen twice still shows once.
 t=event(t,{kind:"queue",queued:[],sent:{id:"again",text:"c"}});
 t=event(t,{kind:"queue",queued:[],sent:{id:"again",text:"c"}});
 assert.equal(t.pending.length,1);
 // Couldn't send: it's back in the queue, with why.
 t=event(t,{kind:"queue",queued:[queued("c")],unsent:"again",error:"offline"});
 assert.equal(t.pending.length,0);
 assert.equal(t.queued.length,1);
 assert.equal(t.sendError,"offline");
});
test("queue events decode, and malformed ones are dropped",()=>{
 const e=decodeChatEvent("chat.queue",{sessionKey:"s",queued:[queued("a")],sent:{id:"x",text:"a"},accepted:{id:"x",receipt:{runId:"r",status:"started"}},unsent:"y",error:"e"});
 assert.equal(e.kind,"queue");
 assert.deepEqual([e.sent.id,e.accepted.receipt.runId,e.unsent,e.error],["x","r","y","e"]);
 assert.equal(decodeChatEvent("chat.queue",{sessionKey:"s"}),null);
 assert.equal(decodeChatEvent("chat.queue",{sessionKey:"s",queued:[],accepted:{id:"x",receipt:{}}}).accepted,undefined);
});

// OpenClaw: runId = the send's idempotencyKey; the user row names neither, only `<id>:user`.
const openclawRow=(id,sendId,text="hello",timestamp=2002)=>({role:"user",content:[{type:"text",text}],timestamp,
 __openclaw:{id,...(sendId?{idempotencyKey:`${sendId}:user`}:{})}});
const openclawReceipt=(t,id)=>reduceThread(t,{type:"receipt",id,receipt:{runId:id,status:"started"}});

test("OpenClaw's user row confirms its send by key, before or after the receipt, live or in history",()=>{
 let t=nativeSend(blank("g|s"),"send");
 t=event(t,{kind:"message",message:openclawRow("row","send")});
 assert.equal(t.pending.length,0);
 t=openclawReceipt(nativeSend(blank("g|s"),"send"),"send");
 assert.equal(t.pending.length,1);
 t=history(t,[openclawRow("row","send")]);
 assert.equal(t.pending.length,0);
 assert.equal(toItems(t.messages,t.pending.map(p=>p.message)).length,1);
});

test("another client's OpenClaw row never confirms ours, even with the same text",()=>{
 let t=openclawReceipt(nativeSend(blank("g|s"),"send"),"send");
 t=event(t,{kind:"message",message:openclawRow("phone","phone-send")});
 assert.equal(t.pending.length,1);
 t=event(t,{kind:"message",message:openclawRow("ours","send")});
 assert.equal(t.pending.length,0);
});

test("an OpenClaw row without a key matches its accepted send by text, despite the gateway's clock",()=>{
 let t=history(blank("g|s"),[openclawRow("before",undefined,"hello",500)]);
 t=nativeSend(t,"send","hello",30_000);
 t=event(t,{kind:"message",message:openclawRow("too-old",undefined,"hello",-40_000)});
 t=event(t,{kind:"message",message:openclawRow("other",undefined,"something else",29_000)});
 assert.equal(t.pending.length,1);
 t=openclawReceipt(t,"send");
 assert.equal(t.pending.length,1);
 t=event(t,{kind:"message",message:openclawRow("behind",undefined,"hello",29_000)});
 assert.equal(t.pending.length,0);
});
