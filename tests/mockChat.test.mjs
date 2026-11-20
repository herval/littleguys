import { test } from "node:test";
import assert from "node:assert/strict";
import { mockBackend } from "../src/lib/mock.ts";
import { createChatStore } from "../src/stores/chat.ts";

test("the browser mock confirms sends and uses the same IDs in events and history",async(t)=>{
 t.mock.timers.enable({apis:["setInterval","setTimeout"]});
 const gateway="mock-test",session="agent:test:main",key=`${gateway}|${session}`;
 const call=(method,params)=>mockBackend.invoke("gw_call",{gateway,method,params});
 const settle=(promise)=>{t.mock.timers.tick(120);return promise;};
 const store=createChatStore({
   history:()=>call("chat.history",{sessionKey:session}),
   send:(_g,_s,message,_attachments,_cwd,idempotencyKey)=>call("chat.send",{sessionKey:session,message,idempotencyKey}),
   abort:async()=>{},
 },async()=>{});
 const unlisten=await mockBackend.listen("gw:event",frame=>store.getState().handleEvent(frame.gateway,frame.event,frame.payload));
 t.after(unlisten);
 await settle(store.getState().send(key,"/help",[]));
 let thread=store.getState().threads[key];
 assert.equal(thread.pending.length,0);
 assert.equal(thread.messages.length,1);
 const userId=thread.messages[0].__openclaw.id;
 await settle(store.getState().reload(key));
 assert.equal(store.getState().threads[key].messages[0].__openclaw.id,userId);
 t.mock.timers.tick(3000);
 await settle(store.getState().reload(key));
 thread=store.getState().threads[key];
 assert.equal(thread.live,null);
 assert.equal(thread.pending.length,0);
 assert.deepEqual(thread.messages.map(m=>m.role),["user","assistant"]);
 assert.equal(thread.messages[0].__openclaw.id,userId);
});
