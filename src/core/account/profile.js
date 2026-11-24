import Permit from './permit'
import Api from './api'
import Arrays from '../../utils/arrays'
import Utils from '../../utils/utils'
import Listener from './listener'
import Storage from '../storage/storage'
import Template from '../../interaction/template'
import Head from '../../interaction/head/head'
import ParentalControl from '../../interaction/parental_control'
import Controller from '../controller'
import Select from '../../interaction/select'
import Noty from '../../interaction/noty'
import Loading from '../../interaction/loading'
import Lang from '../lang'
import Advert from './modal'
import Input from '../../interaction/settings/input'
import Bell from '../../interaction/bell'

let profile_icon

function profileIconText(icon){
    if(typeof icon !== 'string') return ''

    let value = icon.trim()

    if(!value || /^l_\d+$/i.test(value)) return ''

    return value
}

function init(){
    let setting = window.lampa_settings.head_profile
    if(setting === '0' || setting === 0 || setting === false || setting === 'false') return

    profile_icon = Template.elem('div', {class: 'head__action selector open--profile'})

    profile_icon.on('hover:enter', select.bind(null, Controller.toggle.bind(Controller, 'head')))

    Head.render().find('.full--screen').before(profile_icon)

    Storage.listener.follow('change',(e)=>{
        if(e.name == 'account' || e.name == 'protocol') update()
    })

    ParentalControl.add('account_profiles',{
        title: 'account_profiles'
    })

    if(Permit.isCub && !Permit.token) update()
}

/**
 * Обновляет иконку профиля
 * @returns {void}
 */
function update(){
    profile_icon.empty()

    if(Permit.access){
        let icon = profileIconText(Permit.account.profile ? Permit.account.profile.icon : '')

        if(icon) profile_icon.append(Template.elem('span', {class: 'head__profile-emoji', text: icon}))
        else profile_icon.append(Template.js('icon_profile'))
    }
    else profile_icon.append(Template.js('icon_profile'))
}
/**
 * Возвращает иконку профиля
 * @returns {string}
 */
function icon(){
    return Permit.token ? Utils.protocol() + Manifest.cub_domain + '/img/profiles/' + (Permit.account.profile.icon || 'l_1') + '.png' : ''
}

/**
 * Проверяет наличие профиля, если его нет - загружает
 * @param {function} call - вызывается если профиль есть
 * @returns {void}
 */
function check(call){
    if(Permit.access){
        Api.load('profiles/all', {attempts: 3}).then((result)=>{
            let account = Permit.account

            if(account.profile.id){
                let active = result.profiles.find(p=>p.id == account.profile.id)

                if(active) account.profile = active
            }
            else{
                let main = result.profiles.find(p=>p.main)

                if(main) account.profile = main
            }

            Storage.set('account', account, true)

            // Переключение на детский источник
            Listener.send('profile_check', {profile: account.profile})

            call()
        }).catch(call)
    }
    else{
        Storage.set('account_user','', true)
    }
}

/**
 * Выбор профиля
 * @param {string} callback - вызывается после выбора
 * @returns {void}
 */
function select(callback){
    ParentalControl.personal('account_profiles',()=>{
        if(Permit.isCub && !Permit.token) return Advert.account()

        let account = Permit.account
        if(!account.profile) account.profile = {id: 0}

        Loading.start(()=>{
            Api.clear()

            Loading.stop()
        })

        Api.load('profiles/all').then((result)=>{
            Loading.stop()

            if(result.secuses){
                let items = Arrays.clone(result.profiles)
                let clone = Arrays.clone(result.profiles)

                items.sort((a, b) => (a.sort || 0) - (b.sort || 0))
                clone.sort((a, b) => (a.sort || 0) - (b.sort || 0))
                
                let list = items.map((elem, index)=>{
                    let icon = profileIconText(elem.icon)

                    elem.title    = elem.name
                    elem.template = 'selectbox_icon'
                    elem.icon     = Permit.isCub
                        ? '<img src="' + Utils.protocol() + Manifest.cub_domain + '/img/profiles/' + elem.icon + '.png" class="size-medium" />'
                        : (icon || Template.string('icon_profile'))
                    elem.clone    = clone[index]
                    elem.subtitle = elem.main ? Lang.translate('account_profile_main') : elem.child ? Lang.translate('account_profile_child') + ' ' + '(' + Lang.translate('filter_rating_to') + ' ' + (elem.age || 12) + ')' : ''

                    elem.selected = account.profile.id == elem.id

                    return elem
                })

                
                let select  = []
                let maximum = Lampa.Account.hasPremium() ? 8 : 3

                let additional = {
                    title: Lang.translate('account_profile_add'),
                    template: 'selectbox_icon',
                    icon: '<svg class="size-medium"><use xlink:href="#sprite-plus"></use></svg>',
                    ghost: list.length >= maximum,
                    onSelect: (a)=>{
                        callback && callback()

                        if(a.ghost){
                            Noty.show(Lang.translate('account_profile_limited').replace('{count}', maximum))
                        }
                        else{
                            let controller = Controller.enabled().name

                            Input.edit({
                                title: Lampa.Lang.translate('account_profile_name'),
                                value: '',
                                free: true,
                                nosave: true,
                                nomic: true
                            },(name)=>{
                                Controller.toggle(controller)

                                if(name){
                                    name = Utils.capitalizeFirstLetter(name)

                                    Api.load('profiles/create', {}, {name}).then((result)=>{
                                        Bell.push({text: Lang.translate('account_profile_created')})
                                    }).catch((e)=>{
                                        Noty.show(Lang.translate(e.decode_code == 200 ? 'account_profile_limited' : 'network_error').replace('{count}', maximum))
                                    })
                                }
                            })
                        }
                    }
                }
                
                select = [additional].concat(list)

                Select.show({
                    title: Lang.translate('account_profiles'),
                    items: list,
                    onSelect: (a)=>{
                        account.profile = a.clone

                        Storage.set('account', account)

                        callback && callback()

                        Listener.send('profile_select', {profile: account.profile})
                    },
                    onBack: callback,
                })
            }
            else{
                Noty.show(result.text)

                callback && callback()
            }
        }).catch(()=>{
            Loading.stop()
            
            Noty.show(Lang.translate('account_profiles_empty'))
        })
    }, false, true)
}

export default {
    init: Utils.onceInit(init),
    update,
    check,
    select,
    icon
}
