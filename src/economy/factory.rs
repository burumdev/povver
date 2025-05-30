use std::{
    thread,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast as tokio_broadcast;

use crate::{
    app_state::{FactoryStateData, EconomyStateData, TimerStateData},
    simulation::{
        SimInt,
        SimFlo,
        StateAction,
        hub_comms::*,
        speed::Speed,
        Percentage,
        sim_constants::*
    },
    economy::{
        economy_types::{Money, ProductDemand},
        products::Product,
    },
    logger::{LogMessage, Logger, LogLevel::*},
    utils_data::ReadOnlyRwLock,
    utils_traits::AsFactor,
};
use crate::simulation::TickDuration;

struct ProductionRun {
    demand: ProductDemand,
    units: SimInt,
    cost: Money,
    energy_needed: SimInt,
}

pub struct Factory {
    state_ro: ReadOnlyRwLock<FactoryStateData>,
    econ_state_ro: ReadOnlyRwLock<EconomyStateData>,
    timer_state_ro: ReadOnlyRwLock<TimerStateData>,
    ui_log_sender: tokio_broadcast::Sender<LogMessage>,
    wakeup_receiver: tokio_broadcast::Receiver<StateAction>,
    hub_broadcast_receiver: BroadcastDynReceiver,
    dynamic_sender: BroadcastDynSender,
    dynamic_receiver: DynamicReceiver,
    production_runs: Vec<ProductionRun>,
    last_hundred_energy_purchases: Vec<EnergyReceipt>,
    product_demand_sell_threshold: Percentage,
    profit_margin: Percentage,
    sleeptime: Duration,
}

impl Factory {
    pub fn new(
        state_ro: ReadOnlyRwLock<FactoryStateData>,
        econ_state_ro: ReadOnlyRwLock<EconomyStateData>,
        timer_state_ro: ReadOnlyRwLock<TimerStateData>,
        ui_log_sender: tokio_broadcast::Sender<LogMessage>,
        wakeup_receiver: tokio_broadcast::Receiver<StateAction>,
        hub_broadcast_receiver: tokio_broadcast::Receiver<DynamicSignal>,
        dynamic_sender: BroadcastDynSender,
        dynamic_receiver: DynamicReceiver,
    ) -> Self {
        Self {
            state_ro,
            econ_state_ro,
            timer_state_ro,
            ui_log_sender,
            wakeup_receiver,
            hub_broadcast_receiver,
            dynamic_sender,
            dynamic_receiver,
            production_runs: Vec::new(),
            last_hundred_energy_purchases: Vec::new(),
            product_demand_sell_threshold: Percentage::new(0.0),
            profit_margin: Percentage::new(20.0),
            sleeptime: Self::recalculate_sleeptime(Speed::NORMAL.get_tick_duration()),
        }
    }
}

impl Factory {
    fn recalculate_sleeptime(tick_duration: TickDuration) -> Duration {
        // tick durations are in milliseconds so we multiply with 1k to get micros
        //TODO: Add a random number to the tail of this calculation so that factory loop
        // cycles won't overlap in time.
        let micros = (tick_duration / 2) * 1000 - 100;

        Duration::from_micros(micros)
    }

    fn product_in_prod_run(&self, product: &Product) -> Option<usize> {
        self.production_runs.iter().position(|run| run.demand.product == product)
    }

    fn product_in_stock(&self, product: &Product) -> Option<usize> {
        self.state_ro.read().unwrap().product_stocks.iter().position(|stock| stock.product == product)
    }

    fn maybe_produce_goods(&mut self) {
        let (factory_id, balance, available_energy, producable_demands) = {
            let econ_state_ro = self.econ_state_ro.read().unwrap();
            let state_ro = self.state_ro.read().unwrap();
            let producable_demands = econ_state_ro
                .product_demands
                .iter()
                .copied()
                .filter(
                    |demand|
                        demand.product.industry == state_ro.industry && state_ro.product_portfolio.contains(&demand.product) &&
                        self.product_in_prod_run(demand.product).is_none() && self.product_in_stock(demand.product).is_none()
                )
                .collect::<Vec<_>>();

            (
                state_ro.id,
                state_ro.balance,
                state_ro.available_energy.val(),
                producable_demands,
            )
        };

        if producable_demands.len() > 0 {
            for demand in producable_demands {
                let product = &demand.product;

                let unit_cost_ex_energy = product.get_unit_cost_excl_energy();
                // If the factory can't even produce a single unit of the product,
                // It probably has gone bankrupt here.
                if balance.val() < unit_cost_ex_energy {
                    self.log_ui_console(format!("Can't produce even a single unit of {}", product.name), Critical);
                    self.dynamic_sender.send(Arc::new(FactoryHubSignal::DeclaringBankrupcy)).unwrap();

                    return;
                }

                let demand_units = demand.as_units();

                let budget = balance.val() * 0.75;

                let mut budget_units = (budget / unit_cost_ex_energy) as SimInt;
                // Don't produce more than the demand.
                budget_units = budget_units.clamp(0, demand_units);

                // If we can produce at least one percent of the demand, we'll do it.
                if budget_units > product.demand_info.unit_per_percent {
                    let cost = Money::new(budget_units as SimFlo * unit_cost_ex_energy);
                    let energy_needed = budget_units * (product.unit_production_cost.energy - available_energy);
                    self.production_runs.push(ProductionRun {
                        demand,
                        units: budget_units,
                        cost,
                        energy_needed
                    });
                    if energy_needed > 0 {
                        let energy_demand = FactoryEnergyDemand {
                            factory_id,
                            energy_needed,
                        };

                        self.dynamic_sender.send(Arc::new(
                            FactoryHubSignal::EnergyDemand(
                                energy_demand,
                            ))
                        ).unwrap();
                    } else {
                        self.produce_product_demand(demand, budget_units, unit_cost_ex_energy);
                    }
                }
            }
        }
    }

    fn evaluate_pp_energy_offer(&mut self, offer: &PPEnergyOffer) {
        let balance = self.state_ro.read().unwrap().balance;

        if !self.production_runs.is_empty() {
            //TODO: A more sophisticated algo to evaluate the price here might be better option.
            // For now we just accept whatever comes from pp
            let prun = self.production_runs.last_mut().unwrap();
            let energy_cost = offer.price_per_unit.val() * offer.units as SimFlo;
            let remaining_budget = balance.val() - (prun.cost.val() + energy_cost);
            if remaining_budget.val() > 0.0 {
                prun.cost.inc(energy_cost.val());

                self.dynamic_sender.send(Arc::new(FactorySignal::AcceptPPEnergyOffer(*offer))).unwrap();
            } else {
                self.dynamic_sender.send(Arc::new(FactorySignal::RejectPPEnergyOffer(*offer))).unwrap();
            }
        }
    }

    fn energy_received(&mut self) {
        let (balance, energy_available) = {
            let state_ro = self.state_ro.read().unwrap();
            (state_ro.balance, state_ro.available_energy)
        };

        if let Some(index) = self.production_runs.iter().rposition(|run| {
            run.energy_needed <= energy_available.val() && run.cost <= balance
        }) {
            let run = &self.production_runs[index];
            let unit_cost = run.cost.val() / run.units as SimFlo;
            // Demand units might be different from run units
            let units = run.units.clamp(0, run.demand.units);
            self.produce_product_demand(run.demand, units, unit_cost);
        }
    }

    fn produce_product_demand(&mut self, demand: ProductDemand, units: SimInt, unit_cost: SimFlo) {
        self.log_ui_console(format!("Producing {} units of {} for a demand of {} units.", units, demand.product.name, demand.units), Info);
        self.dynamic_sender.send(
            Arc::new(FactoryHubSignal::ProducingProductDemand(demand, units, unit_cost))
        ).unwrap();
    }

    fn maybe_sell_goods(&self) {
        let state_ro = self.state_ro.read().unwrap();
        let econ_state_ro = self.econ_state_ro.read().unwrap();
        state_ro.product_stocks.iter().enumerate().for_each(|(stock_index, stock)| {
            if econ_state_ro.product_demands.iter().position(|demand|
                demand.product == stock.product && demand.percent.val() > self.product_demand_sell_threshold.val()
            ).is_some() {
                let unit_price = stock.unit_production_cost.val() + stock.unit_production_cost.val() * self.profit_margin.as_factor();
                self.dynamic_sender.send(Arc::new(FactoryHubSignal::SellingProduct(stock_index, unit_price.into()))).unwrap();
            }
        })
    }

    fn production_complete(&mut self, receipt: &ProductionReceipt) {
        let demand_index = self.production_runs.iter().position(|run| run.demand == receipt.demand);
        if let Some(remove_index) = demand_index {
            self.production_runs.remove(remove_index);
            self.maybe_sell_goods();
        }
    }

    fn maybe_buy_renewables(&self) {
        //TODO: More detailed algo for renewable buying
        //TODO: Add wind turbines here
        let (current_solarpanels_count, balance, is_awaiting_solarpanels) = {
            let state = self.state_ro.read().unwrap();
            (
                state.solarpanels.len(),
                state.balance,
                state.is_awaiting_solarpanels,
            )
        };

        if balance.val() >= 50000.0 && !is_awaiting_solarpanels {
            let budget = balance.val() - 50000.0;
            let max_solar_panels = (budget / SOLAR_PANEL_PRICE) as usize;

            if max_solar_panels > 0 {
                let can_buy_count = FACTORY_MAX_SOLAR_PANELS - current_solarpanels_count;
                let amount = if max_solar_panels <= can_buy_count { max_solar_panels } else { can_buy_count };

                self.dynamic_sender.send(Arc::new(FactoryHubSignal::BuyingSolarPanels(amount))).unwrap();
            }
        }
    }
}

impl Factory {
    pub fn start(me: Arc<Mutex<Self>>) -> thread::JoinHandle<()> {
        let (
            my_id,
            state_ro,
            mut wakeup_receiver,
            mut hub_broadcast_receiver,
            dynamic_receiver
        ) = {
            let me_lock = me.lock().unwrap();
            (
                me_lock.state_ro.read().unwrap().id,
                ReadOnlyRwLock::clone(&me_lock.state_ro),
                me_lock.wakeup_receiver.resubscribe(),
                me_lock.hub_broadcast_receiver.resubscribe(),
                me_lock.dynamic_receiver.clone(),
            )
        };

        thread::Builder::new().name("POVVER_F_".to_string() + &my_id.to_string()).spawn(move || {
            'outer: loop {
                if let Ok(signal) = hub_broadcast_receiver.try_recv() {
                    let signal_any = signal.as_any();
                    match signal_any {
                        s if s.is::<FactoryEnergyDemand>() => {
                            if let Some(demand) = signal_any.downcast_ref::<FactoryEnergyDemand>() {
                                if demand.factory_id != my_id {
                                    //TODO
                                    //me.lock().unwrap().log_console(format!("Got message: {:?} is from another guy :)", signal), Critical);
                                    // MAYBE SELL SOME LEFTOVER ENERGY TO THE FACTORY IN NEED
                                } else {
                                    //TODO
                                    //me.lock().unwrap().log_console(format!("Got message: {:?} is from me haha :)", signal), Critical);
                                }
                            }
                        },
                        _ => ()
                    }
                }

                if let Ok(signal) = dynamic_receiver.try_recv() {
                    let signal_any = signal.as_any();
                    match signal_any {
                        s if s.is::<PPEnergyOffer>() => {
                            if let Some(offer) = signal_any.downcast_ref::<PPEnergyOffer>() {
                                me.lock().unwrap().log_ui_console(format!("Got energy offer from PP: {} units.", offer.units), Info);
                                me.lock().unwrap().evaluate_pp_energy_offer(offer);
                            }
                        },
                        s if s.is::<HubFactorySignal>() => {
                            if let Some(signal_from_hub) = signal_any.downcast_ref::<HubFactorySignal>() {
                                match signal_from_hub {
                                    HubFactorySignal::EnergyTransfered(receipt) => {
                                        me.lock().unwrap().log_ui_console(format!("{} units of energy received.", receipt.units), Info);
                                        me.lock().unwrap().last_hundred_energy_purchases.push(receipt.clone());
                                        me.lock().unwrap().energy_received();
                                    }
                                    HubFactorySignal::ProductionComplete(receipt) => {
                                        me.lock().unwrap().production_complete(&receipt);
                                    }
                                    HubFactorySignal::RenewableEnergyProduced => {
                                        me.lock().unwrap().maybe_produce_goods();
                                    }
                                }
                            }
                        },
                        _ => ()
                    }
                }

                if let Ok(action) = wakeup_receiver.try_recv() {
                    match action {
                        StateAction::Timer(event) => {
                            if state_ro.read().unwrap().is_bankrupt == true {
                                if event.at_least_day() {
                                    me.lock().unwrap().log_ui_console("Gone belly up! We're bankrupt! Pivoting to ball bearing production ASAP!".to_string(), Critical);
                                }
                            } else {
                                if event.at_least_minute() {
                                    let minute = me.lock().unwrap().timer_state_ro.read().unwrap().date.minute;
                                    if minute % 5 == 0 {
                                        me.lock().unwrap().maybe_produce_goods();
                                    } else if minute % 6 == 0 {
                                        me.lock().unwrap().maybe_sell_goods();
                                    }
                                }
                                if event.at_least_hour() {
                                    me.lock().unwrap().maybe_buy_renewables();
                                }
                            }
                        }
                        StateAction::SpeedChange(td) => {
                            me.lock().unwrap().sleeptime = Self::recalculate_sleeptime(td);
                        }
                        StateAction::Quit => {
                            break 'outer;
                        }
                        _ => ()
                    }
                }

                thread::sleep(me.lock().unwrap().sleeptime);
            }
        }).unwrap()
    }
}

impl Logger for Factory {
    fn get_log_prefix(&self) -> String {
        format!("Factory No. {}", self.state_ro.read().unwrap().id)
    }
    fn get_message_source(&self) -> MessageEntity {
        MessageEntity::Factory(self.state_ro.read().unwrap().id as SimInt)
    }
    fn get_log_sender(&self) -> &tokio_broadcast::Sender<LogMessage> {
        &self.ui_log_sender
    }
}
