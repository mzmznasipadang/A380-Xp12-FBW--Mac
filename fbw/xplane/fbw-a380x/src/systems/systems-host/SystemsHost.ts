// Copyright (c) 2021-2026 FlyByWire Simulations
//
// SPDX-License-Identifier: GPL-3.0

import {
  PanelEventPublisher,
  Board,
  SimClock,
  ClockTopics,
  BusSignal,
  Derived,
  Logic,
  WeightPublisher,
  StallPublisher,
  VarUnit,
  Signal,
} from 'xplane-js/ui';
import { LegacyGpws } from './Misc/LegacyGpws';
import { LegacyFuel } from './CpiomF/LegacyFuel';
import { LegacySoundManager } from './Misc/LegacySoundManager';
import { LegacyTcasComputer } from './Misc/tcas/components/LegacyTcasComputer';
import { VhfRadio } from './Misc/Communications/VhfRadio';
import {
  IrBusPublisher,
  ArincEventBus,
  BtvSimvarPublisher,
  FailuresConsumer,
  MsfsFlightModelPublisher,
  MsfsMiscPublisher,
  PilotSeatPublisher,
  VhfComIndices,
  SwitchingPanelPublisher,
  RaBusPublisher,
  LgciuBusPublisher,
} from '@flybywiresim/fbw-sdk';
import { AudioManagementUnit } from './Misc/Communications/AudioManagementUnit';
import { RmpAmuBusPublisher } from './Misc/Communications/RmpAmuBusPublisher';
import { Transponder } from './Misc/Communications/Transponder';
import { PowerSupplyBusTypes, PowerSupplyBusses } from './Misc/powersupply';
import { SimAudioManager } from './Misc/Communications/SimAudioManager';
import { AtsuSystem } from './CpiomD/atsu';
import { FwsCore } from './CpiomC/FlightWarningSystem/FwsCore';
// FIXME should not import from instruments
import { FuelSystemPublisher } from '../instruments/src/MsfsAvionicsCommon/providers/FuelSystemPublisher';
import { BrakeToVacate } from './PseudoPRIM/BrakeToVacate';
// FIXME should not import from instruments
import { PseudoFwcSimvarPublisher } from '../instruments/src/MsfsAvionicsCommon/providers/PseudoFwcPublisher';
// FIXME should not import from instruments
import { FcdcBusPublisher } from '@shared/publishers/FcdcPublisher';
// FIXME should not import from instruments
import {
  ResetPanelSimvarPublisher,
  ResetPanelSimvars,
} from '../instruments/src/MsfsAvionicsCommon/providers/ResetPanelPublisher';
// FIXME should not import from instruments
import { EgpwcBusPublisher } from '../instruments/src/MsfsAvionicsCommon/providers/EgpwcBusPublisher';
// FIXME should not import from instruments
import { FGDataPublisher } from '../instruments/src/MsfsAvionicsCommon/providers/FGDataPublisher';
// FIXME should not import from instruments
import { AesuBusPublisher } from '../instruments/src/MsfsAvionicsCommon/providers/AesuBusPublisher';
import { A380Failure } from '@failures';
import { EfisTawsBridge } from './Misc/EfisTawsBridge';
// FIXME should not import from ND!!
import { FmsSymbolsPublisher } from '../instruments/src/ND/FmsSymbolsPublisher';
// FIXME should not import from instruments
import { FmsMessagePublisher } from '../shared/src/publishers/FmsMessagePublisher';
import { FqmsBusPublisher } from '@shared/publishers/FqmsBusPublisher';
import { CpiomData, CpiomDataPublisher } from '@providers/CpiomPublisher';
import { AtcDatalink } from './CpiomD/AtcDatalink';
import { Acr } from './CpiomD/Acr';
import { SimVarHandling } from '@datalink/common';
import { PanelInstrument, Vars, defineInstrument, openChannel } from 'xplane-js/data';
import { EcuBusPublisher } from '@shared/publishers/EcuPublisher';

class SystemsHost extends PanelInstrument {
  private readonly bus = new ArincEventBus();

  private readonly sub = this.bus.inbox<PowerSupplyBusTypes & ResetPanelSimvars & CpiomData>();

  private readonly backplane = new Board();

  private readonly clock = new SimClock(this.bus);

  private readonly hEventPublisher: PanelEventPublisher;

  private readonly failuresConsumer = new FailuresConsumer();

  // TODO: Migrate PowerSupplyBusses, if needed
  private gpws: LegacyGpws | undefined;

  private soundManager: LegacySoundManager | undefined;

  private readonly acEssBusPowered = BusSignal.from(this.sub.topic('acBusEss'), false);
  private readonly acBus2Powered = BusSignal.from(this.sub.topic('acBus2'), false);
  private readonly dcEssBusPowered = BusSignal.from(this.sub.topic('dcBusEss'), false);
  private readonly dcBus1Powered = BusSignal.from(this.sub.topic('dcBus1'), false);
  private readonly dcBus2Powered = BusSignal.from(this.sub.topic('dcBus2'), false);

  private readonly vhf1 = new VhfRadio(this.bus, VhfComIndices.Vhf1, 36, this.dcEssBusPowered, this.failuresConsumer);
  private readonly vhf2 = new VhfRadio(this.bus, VhfComIndices.Vhf2, 38, this.dcBus2Powered, this.failuresConsumer);
  private readonly vhf3 = new VhfRadio(this.bus, VhfComIndices.Vhf3, 40, this.dcBus1Powered, this.failuresConsumer);

  // TODO powered subs
  private readonly amu1 = new AudioManagementUnit(this.bus, 1, this.failuresConsumer);
  private readonly amu2 = new AudioManagementUnit(this.bus, 2, this.failuresConsumer);
  private readonly simAudioManager = new SimAudioManager(this.bus, this.amu1, this.amu2);

  private readonly xpdr1 = new Transponder(
    this.bus,
    1,
    41,
    Derived.from(Logic.or(), this.acEssBusPowered, this.acBus2Powered),
    this.failuresConsumer,
  );
  // MSFS only supports 1
  // private readonly xpdr2 = new Transponder(2, 144, this.acBus2Powered, this.failuresConsumer);

  private readonly simVarHandling = new SimVarHandling(this.bus);
  private readonly atsu = new AtsuSystem(this.bus);
  private readonly atc = new AtcDatalink(this.bus);
  private readonly acr = new Acr(this.bus);

  private readonly btv = new BrakeToVacate(this.bus, this);

  private readonly rmpAmuBusPublisher = new RmpAmuBusPublisher(this.bus);

  private readonly pilotSeatPublisher = new PilotSeatPublisher(this.bus);

  private readonly powerPublisher = new PowerSupplyBusses(this.bus);

  private readonly btvPublisher = new BtvSimvarPublisher(this.bus);

  private readonly weightAndBalancePublisher = new WeightPublisher(this.bus);

  private readonly fuelSystemPublisher = new FuelSystemPublisher(this.bus);

  private readonly fqmsPublisher = new FqmsBusPublisher(this.bus);

  private readonly stallWarningPublisher = new StallPublisher(this.bus, 0.9);

  private readonly pseudoFwcPublisher = new PseudoFwcSimvarPublisher(this.bus);

  private readonly fcdcPublisher = new FcdcBusPublisher(this.bus);

  private readonly resetPanelPublisher = new ResetPanelSimvarPublisher(this.bus);

  private readonly cpiomDataPublisher = new CpiomDataPublisher(this.bus);

  private readonly interactivePointsPublisher = new MsfsFlightModelPublisher(this.bus);

  private readonly fmsSymbolsPublisher = new FmsSymbolsPublisher(this.bus, 'L'); // FIXME figure out side dependency
  private readonly egpwcPublisher = new EgpwcBusPublisher(this.bus, 'L');
  private readonly fgDataPublisher = new FGDataPublisher(this.bus);
  private readonly msfsMiscPublisher = new MsfsMiscPublisher(this.bus);
  private readonly irBusPublisher = new IrBusPublisher(this.bus);
  private readonly raBusPublisher = new RaBusPublisher(this.bus);
  private readonly lgciuBusPublisher = new LgciuBusPublisher(this.bus);
  private readonly aesuBusPublisher = new AesuBusPublisher(this.bus);
  private readonly switchingPanelPublisher = new SwitchingPanelPublisher(this.bus);
  private readonly ecuPublisher = new EcuBusPublisher(this.bus);

  private readonly efisTawsBridge = new EfisTawsBridge(this.bus, this, this.failuresConsumer);

  private readonly fmsMessagePublisher = new FmsMessagePublisher(this.bus);

  private readonly fws1ResetPbStatus = BusSignal.from(this.sub.topic('a380x_reset_panel_fws1'), false);
  private readonly fws2ResetPbStatus = BusSignal.from(this.sub.topic('a380x_reset_panel_fws2'), false);

  private readonly fws1Powered = BusSignal.from(this.sub.topic('cpiom_c_available_1'), true);
  private readonly fws2Powered = BusSignal.from(this.sub.topic('cpiom_c_available_2'), true);

  private readonly fws1Failed = Signal.of(false);
  private readonly fws2Failed = Signal.of(false);

  private readonly fws1Healthy = Signal.of(false);
  private readonly fws2Healthy = Signal.of(false);

  private readonly fwsEcpFailed = Signal.of(false);

  private readonly allFwsFailed = Derived.from(
    ([failed1, failed2]) => failed1 && failed2,
    this.fws1Failed,
    this.fws2Failed,
  );

  private fwsCore: FwsCore | undefined = new FwsCore(
    1,
    this.bus,
    this.failuresConsumer,
    this.fws1Failed,
    this.fws2Failed,
  );

  //FIXME add some deltatime functionality to backplane instruments so we dont have to pass SystemHost
  private readonly legacyFuel = new LegacyFuel(this.bus, this);

  /**
   * "mainmenu" = 0
   * "loading" = 1
   * "briefing" = 2
   * "ingame" = 3
   */
  private gameState = 0;

  constructor() {
    super();

    this.backplane.attachDevice('Clock', this.clock);
    this.backplane.attachDevice('Vhf1', this.vhf1, true);
    this.backplane.attachDevice('Vhf2', this.vhf2, true);
    this.backplane.attachDevice('Vhf3', this.vhf3, true);
    this.backplane.attachDevice('Amu1', this.amu1, true);
    this.backplane.attachDevice('Amu2', this.amu2, true);
    this.backplane.attachDevice('SimAudioManager', this.simAudioManager);
    this.backplane.attachDevice('Xpndr1', this.xpdr1, true);
    this.backplane.attachDevice('LegacyFuel', this.legacyFuel);
    this.backplane.attachDevice('BtvDistanceUpdater', this.btv);
    this.backplane.attachDevice('EfisTawsBridge', this.efisTawsBridge);
    this.backplane.attachFeeder('RmpAmuBusPublisher', this.rmpAmuBusPublisher);
    this.backplane.attachFeeder('PilotSeatPublisher', this.pilotSeatPublisher);
    this.backplane.attachFeeder('PowerPublisher', this.powerPublisher);
    this.backplane.attachFeeder('BtvPublisher', this.btvPublisher);
    this.backplane.attachFeeder('Weightpublisher', this.weightAndBalancePublisher);
    this.backplane.attachFeeder('FuelPublisher', this.fuelSystemPublisher);
    this.backplane.attachFeeder('fqmsPublisher', this.fqmsPublisher);
    this.backplane.attachFeeder('StallWarning', this.stallWarningPublisher);
    this.backplane.attachFeeder('PseudoFwc', this.pseudoFwcPublisher);
    this.backplane.attachFeeder('Fcdc', this.fcdcPublisher);
    this.backplane.attachFeeder('ResetPanel', this.resetPanelPublisher);
    this.backplane.attachFeeder('CpiomData', this.cpiomDataPublisher);
    this.backplane.attachFeeder('InteractivePoints', this.interactivePointsPublisher);
    this.backplane.attachFeeder('FmsSymbolsPublisher', this.fmsSymbolsPublisher);
    this.backplane.attachFeeder('EgpwcPublisher', this.egpwcPublisher);
    this.backplane.attachFeeder('FGDataPublisher', this.fgDataPublisher);
    this.backplane.attachFeeder('MsfsMiscPublisher', this.msfsMiscPublisher);
    this.backplane.attachFeeder('IrBusPublisher', this.irBusPublisher);
    this.backplane.attachFeeder('RaBusPublisher', this.raBusPublisher);
    this.backplane.attachFeeder('LgciuBusPublisher', this.lgciuBusPublisher);
    this.backplane.attachFeeder('AesuPublisher', this.aesuBusPublisher);
    this.backplane.attachFeeder('SwitchingPanelPublisher', this.switchingPanelPublisher);
    this.backplane.attachFeeder('fmsMessage', this.fmsMessagePublisher);
    this.backplane.attachFeeder('ecuBus', this.ecuPublisher);

    this.hEventPublisher = new PanelEventPublisher(this.bus);
    this.soundManager = new LegacySoundManager();
    this.gpws = new LegacyGpws(this.bus, this.soundManager);
    this.gpws.init();

    this.backplane.attachDevice('TcasComputer', new LegacyTcasComputer(this.bus, this.soundManager));

    let lastUpdateTime: number;
    this.bus
      .inbox<ClockTopics>()
      .topic('simTimeHiFreq')
      .limit(50)
      .listen((now) => {
        const dt = lastUpdateTime === undefined ? 0 : now - lastUpdateTime;
        lastUpdateTime = now;

        this.soundManager?.update(dt);
        this.gpws?.update(dt);
        this.fwsCore?.update(dt);
        this.powerPublisher.tick();
        this.simVarHandling.tick();
      });

    this.allFwsFailed.watch((a) => {
      if (a && this.fwsCore !== undefined) {
        // Warning: Potential memory leak, if not all subscriptions are collected and cleaned up properly
        this.fwsCore.destroy();
        this.fwsCore = undefined;
        FwsCore.sendFailureWarning(this.bus);
      } else if (!a && this.fwsCore === undefined) {
        this.fwsCore = new FwsCore(1, this.bus, this.failuresConsumer, this.fws1Failed, this.fws2Failed);
      }
    }, true);

    this.fws1Healthy.watch((healthy) => {
      Vars.write('L:A32NX_FWS1_IS_HEALTHY', VarUnit.Bool, healthy);
    }, true);
    this.fws2Healthy.watch((healthy) => {
      Vars.write('L:A32NX_FWS2_IS_HEALTHY', VarUnit.Bool, healthy);
    }, true);

    this.fwsEcpFailed.watch((v) => Vars.write('L:A32NX_FWS_ECP_FAILED', VarUnit.Bool, v), true);
  }

  get templateId(): string {
    return 'A380X_SYSTEMSHOST';
  }

  public getDeltaTime() {
    return this.frameDelta;
  }

  public onPanelEvent(args: string[]): void {
    this.hEventPublisher.dispatchPanelEvent(args[0]);
  }

  public onAttach(): void {
    super.onAttach();

    // Needed to fetch METARs from the sim
    openChannel(
      'nav',
      () => {
        console.log('JS_LISTENER_FACILITY registered.');
      },
      true,
    );

    this.failuresConsumer.register(A380Failure.Fws1);
    this.failuresConsumer.register(A380Failure.Fws2);
    this.failuresConsumer.register(A380Failure.Fws1AudioFunction);
    this.failuresConsumer.register(A380Failure.Fws2AudioFunction);
    this.failuresConsumer.register(A380Failure.FwsEcp);

    this.backplane.setup();
    this.acr.init();
    this.atc.init();
    this.atsu.init();
    this.simVarHandling.initialize();
  }

  public onFrame(): void {
    super.onFrame();

    this.failuresConsumer.update();
    this.fws1Failed.set(
      this.failuresConsumer.isActive(A380Failure.Fws1) || this.fws1ResetPbStatus.get() || !this.fws1Powered.get(),
    );
    this.fws2Failed.set(
      this.failuresConsumer.isActive(A380Failure.Fws2) || this.fws2ResetPbStatus.get() || !this.fws2Powered.get(),
    );

    this.fws1Healthy.set(
      (!this.fws1Failed.get() && this.fws1Powered.get() && this.fwsCore?.startupCompleted.get()) ?? false,
    );
    this.fws2Healthy.set(
      (!this.fws2Failed.get() && this.fws2Powered.get() && this.fwsCore?.startupCompleted.get()) ?? false,
    );

    const ecpNotReachable =
      !Vars.read('L:A32NX_AFDX_3_3_REACHABLE', VarUnit.Bool) &&
      !Vars.read('L:A32NX_AFDX_13_13_REACHABLE', VarUnit.Bool) &&
      !Vars.read('L:A32NX_AFDX_4_4_REACHABLE', VarUnit.Bool) &&
      !Vars.read('L:A32NX_AFDX_14_14_REACHABLE', VarUnit.Bool);
    this.fwsEcpFailed.set(
      this.failuresConsumer.isActive(A380Failure.FwsEcp) || !this.dcEssBusPowered.get() || ecpNotReachable,
    );

    if (this.gameState !== 3) {
      const gamestate = this.sessionState();
      if (gamestate === 3) {
        this.hEventPublisher.begin();
        this.powerPublisher.begin();
        this.simVarHandling.begin();
      }
      this.gameState = gamestate;
    }

    this.backplane.tick();
  }
}

defineInstrument('systems-host', SystemsHost);
