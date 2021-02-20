// This file is part of Substrate.

// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Gilt Pallet
//! A pallet allowing accounts to auction for being frozen and receive open-ended
//! inflation-protection in return.

pub use pallet::*;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

#[frame_support::pallet]
pub mod pallet {
	use sp_std::prelude::*;
	use sp_arithmetic::Perquintill;
	use sp_runtime::traits::{Zero, Saturating, SaturatedConversion};
	use frame_support::traits::{Currency, OnUnbalanced, ReservableCurrency};
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	type BalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;
	type PositiveImbalanceOf<T> =
		<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::PositiveImbalance;
	type NegativeImbalanceOf<T> =
		<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::NegativeImbalance;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		type Currency: ReservableCurrency<Self::AccountId>;

		type AdminOrigin: EnsureOrigin<Self::Origin>;

		type Deficit: OnUnbalanced<PositiveImbalanceOf<Self>>;
		type Surplus: OnUnbalanced<NegativeImbalanceOf<Self>>;

		#[pallet::constant]
		type QueueCount: Get<u32>;

		#[pallet::constant]
		type MaxQueueLen: Get<u32>;

		#[pallet::constant]
		type Period: Get<Self::BlockNumber>;

		#[pallet::constant]
		type MinFreeze: Get<BalanceOf<Self>>;

		#[pallet::constant]
		type IntakePeriod: Get<Self::BlockNumber>;

		#[pallet::constant]
		type MaxIntakeBids: Get<u32>;
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[derive(Clone, Eq, PartialEq, Default, Encode, Decode, RuntimeDebug)]
	pub struct GiltBid<Balance, AccountId> {
		amount: Balance,
		who: AccountId,
	}

	#[derive(Clone, Eq, PartialEq, Default, Encode, Decode, RuntimeDebug)]
	pub struct ActiveGilt<Balance, AccountId, BlockNumber> {
		proportion: Perquintill,
		amount: Balance,
		who: AccountId,
		expiry: BlockNumber,
	}

	pub type ActiveIndex = u32;

	/// The way of determining the net issuance (i.e. after factoring in all maturing frozen funds)
	/// is:
	///
	/// `total_issuance - frozen + proportion * total_issuance`
	#[derive(Clone, Eq, PartialEq, Default, Encode, Decode, RuntimeDebug)]
	pub struct ActiveGiltsTotal<Balance> {
		/// The total amount of funds held in reserve for all active gilts.
		frozen: Balance,
		/// The proportion of funds that the `frozen` balance represents to total issuance.
		proportion: Perquintill,
		/// The total number of gilts issued so far.
		index: ActiveIndex,
		/// The target proportion of gilts within total issuance.
		target: Perquintill,
	}

	#[pallet::storage]
	pub type Queues<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		u32,
		Vec<GiltBid<BalanceOf<T>, T::AccountId>>,
		ValueQuery,
	>;

	#[pallet::storage]
	pub type QueueTotals<T> = StorageValue<_, Vec<(u32, BalanceOf<T>)>, ValueQuery>;

	#[pallet::storage]
	pub type ActiveTotal<T> = StorageValue<_, ActiveGiltsTotal<BalanceOf<T>>, ValueQuery>;

	#[pallet::storage]
	pub type Active<T> = StorageMap<
		_,
		Blake2_128Concat,
		ActiveIndex,
		ActiveGilt<BalanceOf<T>, <T as frame_system::Config>::AccountId, <T as frame_system::Config>::BlockNumber>,
		OptionQuery,
	>;

	#[pallet::event]
	#[pallet::metadata(T::AccountId = "AccountId")]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A bid was successfully placed.
		/// \[ who, amount, duration \]
		BidPlaced(T::AccountId, BalanceOf<T>, u32),
		/// A bid was successfully removed (before being accepted as a gilt).
		/// \[ who, amount, duration \]
		BidRetracted(T::AccountId, BalanceOf<T>, u32),
		/// A bid was accepted as a gilt. The balance may not be released until expiry.
		/// \[ index, expiry, who, amount \]
		GiltIssued(ActiveIndex, T::BlockNumber, T::AccountId, BalanceOf<T>),
		/// An expired gilt has been thawed.
		/// \[ index, who, original_amount, additional_amount \]
		GiltThawed(ActiveIndex, T::AccountId, BalanceOf<T>, BalanceOf<T>),
	}

	// Errors inform users that something went wrong.
	#[pallet::error]
	pub enum Error<T> {
		DurationTooSmall,
		DurationTooBig,
		AmountTooSmall,
		QueueFull,
		/// Gilt index is unknown.
		Unknown,
		/// Not the owner of the gilt.
		NotOwner,
		/// Gilt not yet at expiry date.
		NotExpired,
		/// The given bid for retraction is not found.
		NotFound,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(n: T::BlockNumber) -> Weight {
			if (n % T::IntakePeriod::get()).is_zero() {
				let totals = ActiveTotal::<T>::get();
				if totals.proportion < totals.target {
					let missing = totals.target.saturating_sub(totals.proportion);

					let total_issuance = T::Currency::total_issuance();
					let nongilt_issuance: u128 = total_issuance.saturating_sub(totals.frozen)
						.saturated_into();
					let gilt_issuance = totals.proportion * nongilt_issuance;
					let effective_issuance = gilt_issuance.saturating_add(nongilt_issuance);
					let intake: BalanceOf<T> = (missing * effective_issuance).saturated_into();

					let bids_taken = Self::enlarge(intake, T::MaxIntakeBids::get());
					// TODO: Determine actual weight
					return bids_taken as Weight
				}
			}
			0
		}
	}

	#[pallet::call]
	impl<T:Config> Pallet<T> {
		/// Place a bid.
		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		pub fn place_bid(
			origin: OriginFor<T>,
			#[pallet::compact] amount: BalanceOf<T>,
			duration: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			let queue_count = T::QueueCount::get() as usize;
			ensure!(duration > 0, Error::<T>::DurationTooSmall);
			ensure!(duration <= queue_count as u32, Error::<T>::DurationTooBig);
			ensure!(amount >= T::MinFreeze::get(), Error::<T>::AmountTooSmall);

			QueueTotals::<T>::try_mutate(|qs| -> Result<(), DispatchError> {
				qs.resize(queue_count as usize, (0, Zero::zero()));
				ensure!(qs[queue_count - 1].0 < T::MaxQueueLen::get(), Error::<T>::QueueFull);
				qs[queue_count - 1].0 += 1;
				T::Currency::reserve(&who, amount)?;
				qs[queue_count - 1].1 += amount;
				Ok(())
			})?;
			Self::deposit_event(Event::BidPlaced(who.clone(), amount, duration));
			Queues::<T>::mutate(duration, |q| q.insert(0, GiltBid { amount, who }));

			Ok(().into())
		}

		/// Retract a previously placed bid.
		#[pallet::weight(10_000 + T::DbWeight::get().writes(1))]
		pub fn retract_bid(
			origin: OriginFor<T>,
			#[pallet::compact] amount: BalanceOf<T>,
			duration: u32,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			let queue_count = T::QueueCount::get() as usize;
			ensure!(duration > 0, Error::<T>::DurationTooSmall);
			ensure!(duration <= queue_count as u32, Error::<T>::DurationTooBig);
			ensure!(amount >= T::MinFreeze::get(), Error::<T>::AmountTooSmall);

			let bid = GiltBid { amount, who };
			let new_len = Queues::<T>::try_mutate(duration, |q| -> Result<u32, DispatchError> {
				let pos = q.iter().position(|i| i == &bid).ok_or(Error::<T>::NotFound)?;
				q.remove(pos);
				Ok(q.len() as u32)
			})?;

			QueueTotals::<T>::mutate(|qs| {
				qs.resize(queue_count as usize, (0, Zero::zero()));
				qs[queue_count - 1].0 = new_len;
				qs[queue_count - 1].1 -= bid.amount;
			});

			T::Currency::unreserve(&bid.who, bid.amount);
			Self::deposit_event(Event::BidRetracted(bid.who, bid.amount, duration));

			Ok(().into())
		}

		/// Set target proportion of gilt-funds.
		#[pallet::weight(10_000 + T::DbWeight::get().reads_writes(1,1))]
		pub fn set_target(
			origin: OriginFor<T>,
			#[pallet::compact] target: Perquintill,
		) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin(origin)?;
			ActiveTotal::<T>::mutate(|totals| totals.target = target);
			Ok(().into())
		}

		/// Remove an active ongoing
		#[pallet::weight(10_000 + T::DbWeight::get().reads_writes(1,1))]
		pub fn thaw(
			origin: OriginFor<T>,
			#[pallet::compact] index: ActiveIndex,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			// Look for `index`
			let gilt = Active::<T>::get(index).ok_or(Error::<T>::Unknown)?;
			// If found, check the owner is `who`.
			ensure!(gilt.who == who, Error::<T>::NotOwner);
			let now = frame_system::Module::<T>::block_number();
			ensure!(now >= gilt.expiry, Error::<T>::NotExpired);
			// Remove it
			Active::<T>::remove(index);

			// Multiply the proportion it is by the total issued.
			let total_issuance = T::Currency::total_issuance();
			ActiveTotal::<T>::mutate(|totals| {
				let nongilt_issuance: u128 = total_issuance.saturating_sub(totals.frozen)
					.saturated_into();
				let gilt_issuance = totals.proportion * nongilt_issuance;
				let effective_issuance = gilt_issuance.saturating_add(nongilt_issuance);
				let gilt_value: BalanceOf<T> = (gilt.proportion * effective_issuance).saturated_into();

				totals.frozen = totals.frozen.saturating_sub(gilt.amount);
				totals.proportion = totals.proportion.saturating_sub(gilt.proportion);

				// Remove or mint the additional to the amount using `Deficit`/`Surplus`.
				if gilt_value > gilt.amount {
					// Unreserve full amount.
					T::Currency::unreserve(&gilt.who, gilt.amount);
					let amount = gilt_value - gilt.amount;
					let deficit = T::Currency::deposit_creating(&gilt.who, amount);
					T::Deficit::on_unbalanced(deficit);
				} else if gilt_value < gilt.amount {
					// We take anything reserved beyond the gilt's final value.
					let rest = gilt.amount - gilt_value;
					// `slash` might seem a little aggressive, but it's the only way to do it
					// in case it's locked into the staking system.
					let surplus = T::Currency::slash_reserved(&gilt.who, rest).0;
					T::Surplus::on_unbalanced(surplus);
					// Unreserve only its new value (less than the amount reserved). Everything
					// should add up, but (defensive) in case it doesn't, unreserve takes lower
					// priority over the funds.
					T::Currency::unreserve(&gilt.who, gilt_value);
				}

				let e = Event::GiltThawed(index, gilt.who, gilt.amount, gilt_value);
				Self::deposit_event(e);
			});

			Ok(().into())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Freeze additional funds from queue of bids up to `amount`. Use at most `max_bids`
		/// from the queue.
		///
		/// Return the number of bids taken.
		pub fn enlarge(
			amount: BalanceOf<T>,
			max_bids: u32,
		) -> u32 {
			let total_issuance = T::Currency::total_issuance();
			let mut remaining = amount;
			let mut bids_taken = 0;
			let now = frame_system::Module::<T>::block_number();

			ActiveTotal::<T>::mutate(|totals| {
				QueueTotals::<T>::mutate(|qs| {
					for periods in (1..=T::QueueCount::get()).rev() {
						if qs[periods as usize - 1].0 == 0 {
							continue
						}
						let index = periods as usize - 1;
						let expiry = now + T::Period::get() * periods.into();
						Queues::<T>::mutate(periods, |q| {
							while let Some(mut bid) = q.pop() {
								if remaining < bid.amount {
									let overflow = bid.amount - remaining;
									bid.amount = remaining;
									q.push(GiltBid { amount: overflow, who: bid.who.clone() });
								}
								let amount = bid.amount;
								// Can never overflow due to block above.
								remaining -= amount;
								// Should never underflow since it should track the total of the bids
								// exactly, but we'll be defensive.
								qs[index].1 = qs[index].1.saturating_sub(bid.amount);

								// Now to activate the bid...
								let nongilt_issuance: u128 = total_issuance.saturating_sub(totals.frozen)
									.saturated_into();
								let gilt_issuance = totals.proportion * nongilt_issuance;
								let effective_issuance = gilt_issuance.saturating_add(nongilt_issuance);
								let n: u128 = amount.saturated_into();
								let d = effective_issuance;
								let proportion = Perquintill::from_rational_approximation(n, d);
								let who = bid.who;
								let index = totals.index;
								totals.frozen += bid.amount;
								totals.proportion = totals.proportion.saturating_add(proportion);
								totals.index += 1;
								let e = Event::GiltIssued(index, expiry, who.clone(), amount);
								Self::deposit_event(e);
								let gilt = ActiveGilt { amount, proportion, who, expiry };
								Active::<T>::insert(index, gilt);

								bids_taken += 1;

								if remaining.is_zero() || bids_taken == max_bids {
									break;
								}
							}
							qs[index].0 = q.len() as u32;
						});
						if remaining.is_zero() || bids_taken == max_bids {
							break
						}
					}
				});
			});
			bids_taken
		}
	}
}
