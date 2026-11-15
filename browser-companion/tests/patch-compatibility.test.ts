import {expect,test} from 'bun:test';
const {assertRouteCoordinatorCompatibility}=require('../scripts/build-navigation-stall-patch.cjs');

test('patch preparation rejects a new readiness route against an older coordinator',()=>{
  const route='async function route(coordinator){return flag ? coordinator.readyHealth() : coordinator.health();}';
  expect(()=>assertRouteCoordinatorCompatibility('class SingleWindowCoordinator{health(){}}\n'+route)).toThrow('readyHealth');
  expect(()=>assertRouteCoordinatorCompatibility('class SingleWindowCoordinator{health(){} readyHealth(){}}\n'+route)).not.toThrow();
});

test('patch verification checks calls within nested route branches without treating properties as methods',()=>{
  const route='function route(coordinator){if(flag){return (()=>coordinator.presentation())();}return coordinator.build;}';
  expect(()=>assertRouteCoordinatorCompatibility('class SingleWindowCoordinator{}\n'+route)).toThrow('presentation');
  expect(()=>assertRouteCoordinatorCompatibility('class SingleWindowCoordinator{presentation(){}}\n'+route)).not.toThrow();
});
